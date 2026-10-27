#![cfg(all(target_os = "macos", feature = "fault-injection"))]

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_codemode::CapabilityError;
use cayw_content::{ContentHash, ContentStore, ExpectedRevision, Id, Limits};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_runtime::{
    ContentConfig, ResultStorageIdentity, ResultStorageInspection, ResultStorageObservation,
    ResultStorageState, Runtime, RuntimeConfig, RuntimeError, ScopedPermission,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Once, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

const BODY: &str = "result-body";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    BodyUnknown,
    MetadataUnknown,
    ExitBeforeTerminal,
}
struct Plan {
    mode: Mode,
    prepared: usize,
    writes: usize,
    captures: usize,
    body: String,
    metadata: String,
}
static PLANS: OnceLock<Mutex<BTreeMap<String, Plan>>> = OnceLock::new();
fn plans() -> &'static Mutex<BTreeMap<String, Plan>> {
    PLANS.get_or_init(Mutex::default)
}
fn key(identity: &ResultStorageIdentity, metadata: bool) -> Id {
    let kind = if metadata {
        "cayw-runtime-result-meta-v1"
    } else {
        "cayw-runtime-result-body-v1"
    };
    Id::new(
        ContentHash::of(
            &serde_json::to_vec(&(
                kind,
                &identity.device_id,
                &identity.lease.session_id,
                None::<&str>,
                &identity.handle,
            ))
            .unwrap(),
        )
        .to_string(),
    )
    .unwrap()
}
fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        cayw_ai::faults::install(Arc::new(|point, data| {
            let mut all = plans().lock().unwrap();
            match point {
                "runtime.result_storage.prepared" => {
                    let identity: ResultStorageIdentity = serde_json::from_value(data).unwrap();
                    if let Some(plan) = all.get_mut(&identity.lease.session_id) {
                        plan.prepared += 1;
                        plan.body = key(&identity, false).to_string();
                        plan.metadata = key(&identity, true).to_string();
                    }
                }
                "runtime.shared_read.capture" => {
                    if let Some(plan) = data["lease"]["session_id"]
                        .as_str()
                        .and_then(|session| all.get_mut(session))
                    {
                        plan.captures += 1;
                    }
                }
                "content_state_renamed" => {
                    let name = data["name"].as_str().unwrap();
                    for plan in all.values_mut() {
                        if name == plan.body || name == plan.metadata {
                            plan.writes += 1;
                            if (plan.mode == Mode::BodyUnknown && name == plan.body)
                                || (plan.mode == Mode::MetadataUnknown && name == plan.metadata)
                            {
                                plan.mode = Mode::Normal;
                                assert!(cayw_content::fail_current_state_directory_sync());
                            }
                        }
                    }
                }
                "runtime.result_storage.before_terminal"
                    if data["lease"]["session_id"]
                        .as_str()
                        .and_then(|session| all.get(session))
                        .is_some_and(|plan| plan.mode == Mode::ExitBeforeTerminal) =>
                {
                    // The real process ends after both content writes and before
                    // any storage terminal or original Cells acknowledgment.
                    std::process::exit(71);
                }
                _ => {}
            }
        }))
        .unwrap()
    });
}
fn arm(session: &str, mode: Mode) {
    install();
    plans().lock().unwrap().insert(
        session.into(),
        Plan {
            mode,
            prepared: 0,
            writes: 0,
            captures: 0,
            body: String::new(),
            metadata: String::new(),
        },
    );
}

struct Script {
    source: String,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let explicit = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { content, .. } => Some(cayw_ai::content_text(content)),
                _ => None,
            })
            .as_deref()
            == Some("explicit next task");
        let message = if explicit {
            let mut message = cayw_ai::AssistantMessage::new(&request.model);
            message.content.push(cayw_ai::Content::text(
                "new task; original tool is not replayed",
            ));
            message.stop_reason = cayw_ai::StopReason::Stop;
            message
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                &self.source,
                &format!("externalize-{n}"),
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
struct Effect {
    path: PathBuf,
    calls: AtomicUsize,
}
fn payload() -> Value {
    json!({"payload": BODY.repeat(4000)})
}
#[async_trait]
impl cayw_host::ExternalHost for Effect {
    async fn call(
        &self,
        name: &str,
        args: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        assert_eq!(name, "search.query");
        assert_eq!(args, json!({"query":"externalize"}));
        assert_eq!(
            self.calls.fetch_add(1, Ordering::SeqCst),
            0,
            "original effect repeated"
        );
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.path)
            .unwrap();
        file.write_all(b"one durable effect").unwrap();
        file.sync_all().unwrap();
        std::fs::File::open(self.path.parent().unwrap())
            .unwrap()
            .sync_all()
            .unwrap();
        Ok(payload())
    }
}
fn setup() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    (temp, root)
}
fn config(root: &Path, shared: bool) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "externalization-device".into(),
            limits: Limits {
                max_content_bytes: 64 * 1024,
                ..Default::default()
            },
        }),
        auto_externalize_threshold_bytes: 1024,
        permissions: vec![if shared {
            ScopedPermission::exact("fs.read", json!({"path":"large.txt"}))
        } else {
            ScopedPermission::exact("search.query", json!({"query":"externalize"}))
        }],
        ..Default::default()
    };
    config.host.frozen_reads = shared;
    config
}
fn provider(shared: bool, assertion: &str) -> Arc<Script> {
    let call = if shared {
        "cayw.call('fs.read',{path='large.txt'})"
    } else {
        "cayw.call('search.query',{query='externalize'})"
    };
    Arc::new(Script {
        source: format!("local res = {call}; {assertion}; return res"),
        calls: AtomicUsize::new(0),
    })
}
fn open(root: &Path, provider: Arc<Script>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "externalization"),
        config,
    )
    .unwrap()
}
fn original(rt: &Runtime, session: &str) -> cayw_core::CapabilityCallRecord {
    let calls: Vec<_> = rt
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call }
                if call.name == "fs.read" || call.name == "search.query" =>
            {
                Some(call)
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "exactly one original terminal");
    calls.into_iter().next().unwrap()
}
fn assert_original(rt: &Runtime, root: &Path, session: &str) -> cayw_core::CapabilityCallRecord {
    let call = original(rt, session);
    let Some(CapabilityOutcome::Completed { result }) = &call.outcome else {
        panic!("original Host success changed: {:?}", call.outcome)
    };
    assert!(result.get("externalized").is_none());
    let db = rusqlite::Connection::open(root.join("sessions.sqlite")).unwrap();
    let body: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE session=?1 AND id=?2",
            [session, &call.key.execution_id],
            |row| row.get(0),
        )
        .unwrap();
    let cell: cayw_cells::CellRecord = serde_json::from_str(&body).unwrap();
    assert_eq!(cell.calls.len(), 1);
    let intent = &cell.calls[0];
    assert_eq!(intent.status, cayw_cells::IntentStatus::Succeeded);
    assert_eq!(intent.name, call.name);
    assert_eq!(intent.arguments, call.arguments);
    assert_eq!(
        intent.result_hash.as_deref(),
        Some(cayw_cells::HostResult::value_hash(result).as_str())
    );
    assert_eq!(intent.result.as_ref(), Some(result));
    call
}
async fn assert_blocked(rt: &Runtime, session: &str, task: &str) {
    assert_eq!(
        rt.get(session).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(matches!(
        rt.enqueue(session, "blocked"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert!(rt.resume(task, CancellationToken::new()).await.is_err());
    assert!(matches!(
        rt.fork(session, "confirmed fork", true),
        Err(RuntimeError::NeedsReconciliation)
    ));
}
fn inspect(rt: &Runtime, session: &str) -> ResultStorageInspection {
    let pending = rt.pending_result_storage(session).unwrap();
    assert_eq!(pending.len(), 1);
    rt.inspect_result_storage(&pending[0].identity).unwrap()
}
fn verify_receipt(root: &Path, inspection: &ResultStorageInspection) -> PathBuf {
    let path = root.join("trusted-storage-receipt.json");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.write_all(
        &serde_json::to_vec(&json!({
            "identity": inspection.pending.identity,
            "observation": inspection.observation,
            "body": inspection.body.as_ref().map(|content| &content.record),
            "metadata": inspection.metadata.as_ref().map(|content| &content.record),
        }))
        .unwrap(),
    )
    .unwrap();
    file.sync_all().unwrap();
    std::fs::File::open(root).unwrap().sync_all().unwrap();
    path
}
fn reconcile(rt: &Runtime, root: &Path, inspection: &ResultStorageInspection) {
    let receipt = verify_receipt(root, inspection);
    rt.reconcile_result_storage(inspection, &cayw_requests::HostActor::new("fixture-host").unwrap(), receipt.to_str().unwrap(), |actual| {
        let recorded: Value = serde_json::from_slice(&std::fs::read(&receipt).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
        let observed = json!({"identity": actual.pending.identity, "observation": actual.observation,
            "body": actual.body.as_ref().map(|content| &content.record),
            "metadata": actual.metadata.as_ref().map(|content| &content.record)});
        if recorded != observed { return Err("receipt differs from exact storage".into()); }
        if let Some(body) = &actual.body
            && ContentHash::of(&body.bytes).to_string() != actual.pending.identity.content_hash
        {
            return Err("wrong content".into());
        }
        Ok(())
    }).unwrap();
}

async fn unknown_case(shared: bool, mode: Mode) {
    let (_temp, root) = setup();
    std::fs::write(root.join("workspace/large.txt"), BODY.repeat(4000)).unwrap();
    let effect = Arc::new(Effect {
        path: root.join("effect"),
        calls: AtomicUsize::new(0),
    });
    let mut cfg = config(&root, shared);
    if !shared {
        cfg.host.external = Some(effect.clone());
    }
    let provider = provider(
        shared,
        "error('storage unknown must stop before this assertion')",
    );
    let rt = open(&root, provider.clone(), cfg.clone());
    let session = rt.create(None).unwrap().id;
    arm(&session, mode);
    assert!(matches!(
        rt.prompt(&session, "first task", CancellationToken::new())
            .await,
        Err(RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            ..
        })
    ));
    rt.wait_idle(&session).await;
    let before = assert_original(&rt, &root, &session);
    assert!(
        rt.store()
            .settle_capability(
                &before.lease,
                &before.key,
                CapabilityOutcome::OutcomeUnknown {
                    error: "storage issue cannot replace original success".into(),
                }
            )
            .is_err()
    );
    assert_eq!(original(&rt, &session), before);
    let parent = rt
        .get(&session)
        .unwrap()
        .messages
        .into_iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(parent.is_error);
    assert!(
        !parent.outcome_unknown,
        "storage obligation must have one independent owner"
    );
    let task = rt.tasks(&session).unwrap()[0].id.clone();
    assert_blocked(&rt, &session, &task).await;
    let pending = rt.pending_result_storage(&session).unwrap();
    assert_eq!(pending[0].state, ResultStorageState::OutcomeUnknown);
    {
        use cayw_cells::{CellIdentity, ExecutorStopped, SqliteJournal};
        let journal = Arc::new(SqliteJournal::open(root.join("sessions.sqlite")).unwrap());
        let owner = journal
            .stopped_scope(
                ExecutorStopped {
                    identity: CellIdentity {
                        session_id: session.clone(),
                        task_id: before.task_id.clone(),
                        attempt_id: before.lease.attempt_id.clone(),
                        permission_context: format!(
                            "{}:{}",
                            before.lease.worker_id, before.lease.fence
                        ),
                    },
                    evidence: "another host retains the original Cells ownership".into(),
                },
                cfg.cells.clone(),
            )
            .unwrap();
        assert!(rt.inspect_result_storage(&pending[0].identity).is_err());
        drop(owner);
    }
    let model_calls = provider.calls.load(Ordering::SeqCst);
    {
        let all = plans().lock().unwrap();
        let plan = &all[&session];
        assert_eq!(plan.prepared, 1);
        assert_eq!(plan.writes, if mode == Mode::BodyUnknown { 1 } else { 2 });
        assert_eq!(plan.captures, usize::from(shared));
    }
    assert_eq!(effect.calls.load(Ordering::SeqCst), usize::from(!shared));
    drop(rt);
    let rt = open(&root, provider.clone(), cfg.clone());
    assert_blocked(&rt, &session, &task).await;
    assert_eq!(
        rt.recover(&session).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_blocked(&rt, &session, &task).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), model_calls);
    assert_eq!(assert_original(&rt, &root, &session), before);
    let inspection = inspect(&rt, &session);
    assert_eq!(
        inspection.observation,
        if mode == Mode::BodyUnknown {
            ResultStorageObservation::BodyOnly
        } else {
            ResultStorageObservation::Complete
        }
    );
    let actor = cayw_requests::HostActor::new("fixture-host").unwrap();
    for changed in 0..5 {
        let mut wrong = inspection.clone();
        match changed {
            0 => wrong.pending.identity.task_id.push_str("-wrong"),
            1 => wrong.pending.identity.handle.push('a'),
            2 => wrong.pending.identity.content_hash = ContentHash::of(b"wrong").to_string(),
            3 => wrong.pending.identity.lease.worker_id.push_str("-wrong"),
            _ => wrong.body.as_mut().unwrap().bytes.push(b'x'),
        }
        assert!(
            rt.reconcile_result_storage(&wrong, &actor, "not enough on its own", |_| panic!(
                "wrong identity/content reached verifier"
            ))
            .is_err()
        );
    }
    assert!(
        rt.reconcile_result_storage(&inspection, &actor, "reason alone", |_| Err(
            "trusted host has no receipt".into()
        ))
        .is_err()
    );
    assert_blocked(&rt, &session, &task).await;
    reconcile(&rt, &root, &inspection);
    assert!(rt.pending_result_storage(&session).unwrap().is_empty());
    let recovered = rt.recover(&session).unwrap();
    assert_eq!(recovered.state, SessionState::Idle, "{recovered:#?}");
    assert!(
        !recovered.messages.iter().any(
            |message| matches!(message, Message::ToolResult(result) if result.outcome_unknown)
        ),
        "recovered original replies: {:?}",
        recovered
            .messages
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .collect::<Vec<_>>()
    );
    assert_eq!(assert_original(&rt, &root, &session), before);
    let log = rt.events(&session, 0, 1000).unwrap();
    assert_eq!(log.iter().filter(|event| matches!(&event.event, CoreEvent::Host { kind, .. } if kind == "runtime_result_storage_prepared")).count(), 1);
    assert_eq!(log.iter().filter(|event| matches!(&event.event, CoreEvent::Host { kind, .. } if kind == "runtime_result_storage_terminal")).count(), 1);
    assert_eq!(log.iter().filter(|event| matches!(&event.event, CoreEvent::Host { kind, .. } if kind == "runtime_result_storage_reconciled")).count(), 1);
    assert!(
        rt.reconcile_result_storage(&inspection, &actor, "repeat", |_| Ok(()))
            .is_err()
    );
    drop(rt);
    let rt = open(&root, provider, cfg);
    assert!(rt.pending_result_storage(&session).unwrap().is_empty());
    assert_eq!(
        rt.get(&session).unwrap().state,
        SessionState::Idle,
        "reopen state: {:#?}",
        rt.get(&session).unwrap()
    );
    assert_eq!(
        rt.prompt(&session, "explicit next task", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    assert_eq!(effect.calls.load(Ordering::SeqCst), usize::from(!shared));
    assert_eq!(original(&rt, &session), before);
}

#[tokio::test]
async fn ordinary_effect_success_survives_body_commit_unknown_and_host_reconciliation() {
    unknown_case(false, Mode::BodyUnknown).await;
}
#[tokio::test]
async fn shared_read_success_survives_metadata_commit_unknown_and_host_reconciliation() {
    unknown_case(true, Mode::MetadataUnknown).await;
}

#[tokio::test]
async fn normal_externalization_stores_original_in_cells_and_delivers_one_handle_to_luau() {
    let (_temp, root) = setup();
    std::fs::write(root.join("workspace/large.txt"), BODY.repeat(4000)).unwrap();
    let rt = open(
        &root,
        provider(
            true,
            "assert(res.externalized == true and res.handle ~= nil)",
        ),
        config(&root, true),
    );
    let session = rt.create(None).unwrap().id;
    arm(&session, Mode::Normal);
    rt.prompt(&session, "read", CancellationToken::new())
        .await
        .unwrap();
    rt.wait_idle(&session).await;
    assert_original(&rt, &root, &session);
    assert!(rt.pending_result_storage(&session).unwrap().is_empty());
    let all = plans().lock().unwrap();
    let plan = &all[&session];
    assert_eq!(plan.prepared, 1);
    assert_eq!(plan.writes, 2);
    assert_eq!(plan.captures, 1);
}

#[tokio::test]
async fn known_storage_quota_failure_returns_original_value_without_freezing() {
    let (_temp, root) = setup();
    let effect = Arc::new(Effect {
        path: root.join("effect"),
        calls: AtomicUsize::new(0),
    });
    let mut cfg = config(&root, false);
    cfg.host.external = Some(effect.clone());
    cfg.content.as_mut().unwrap().limits.max_records = 1;
    let rt = open(
        &root,
        provider(
            false,
            "assert(res.payload ~= nil and res.externalized == nil)",
        ),
        cfg,
    );
    let session = rt.create(None).unwrap().id;
    arm(&session, Mode::Normal);
    assert_eq!(
        rt.prompt(&session, "read", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    rt.wait_idle(&session).await;
    assert_original(&rt, &root, &session);
    assert!(rt.pending_result_storage(&session).unwrap().is_empty());
    assert!(rt.fork(&session, "known failure fork", true).is_ok());
    assert_eq!(effect.calls.load(Ordering::SeqCst), 1);
    assert_eq!(plans().lock().unwrap()[&session].prepared, 1);
}

#[tokio::test]
async fn changed_actual_content_cannot_be_reconciled_as_the_prepared_result() {
    let (_temp, root) = setup();
    let effect = Arc::new(Effect {
        path: root.join("effect"),
        calls: AtomicUsize::new(0),
    });
    let mut cfg = config(&root, false);
    cfg.host.external = Some(effect);
    let rt = open(&root, provider(false, "error('unreachable')"), cfg.clone());
    let session = rt.create(None).unwrap().id;
    arm(&session, Mode::BodyUnknown);
    assert!(
        rt.prompt(&session, "read", CancellationToken::new())
            .await
            .is_err()
    );
    rt.wait_idle(&session).await;
    rt.recover(&session).unwrap();
    let inspection = inspect(&rt, &session);
    let content = cfg.content.unwrap();
    let store = ContentStore::open(
        &content.directory,
        Id::new(content.device_id).unwrap(),
        content.limits,
    )
    .unwrap();
    let body = inspection.body.as_ref().unwrap();
    store
        .put(
            &body.record.name,
            body.record.expected(),
            b"different real content",
        )
        .unwrap();
    assert!(
        rt.inspect_result_storage(&inspection.pending.identity)
            .is_err()
    );
    assert!(
        rt.reconcile_result_storage(
            &inspection,
            &cayw_requests::HostActor::new("fixture-host").unwrap(),
            "wrong content",
            |_| panic!("changed content reached verifier")
        )
        .is_err()
    );
    assert_eq!(rt.pending_result_storage(&session).unwrap().len(), 1);
}

#[test]
fn crash_window_child() {
    let Some(root) = std::env::var_os("CAYW_RESULT_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let rt = open(
            &root,
            provider(true, "error('unreachable')"),
            config(&root, true),
        );
        let session = rt.create(None).unwrap().id;
        let mut file = std::fs::File::create(root.join("session.txt")).unwrap();
        file.write_all(session.as_bytes()).unwrap();
        file.sync_all().unwrap();
        arm(&session, Mode::ExitBeforeTerminal);
        let _ = rt.prompt(&session, "read", CancellationToken::new()).await;
        panic!("child failed to reach the real commit window");
    });
}

#[tokio::test]
async fn process_exit_after_writes_preserves_preparation_until_independent_host_decision() {
    let (_temp, root) = setup();
    std::fs::write(root.join("workspace/large.txt"), BODY.repeat(4000)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_window_child", "--nocapture"])
        .env("CAYW_RESULT_CRASH_ROOT", &root)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(71),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session = std::fs::read_to_string(root.join("session.txt")).unwrap();
    let cfg = config(&root, true);
    let provider = provider(true, "error('no replay')");
    let rt = open(&root, provider.clone(), cfg.clone());
    let pending = rt.pending_result_storage(&session).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, ResultStorageState::Prepared);
    let task = rt.tasks(&session).unwrap()[0].id.clone();
    assert_blocked(&rt, &session, &task).await;
    assert_eq!(
        rt.recover(&session).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_blocked(&rt, &session, &task).await;
    let before = assert_original(&rt, &root, &session);
    let inspection = inspect(&rt, &session);
    assert_eq!(inspection.observation, ResultStorageObservation::Complete);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    reconcile(&rt, &root, &inspection);
    rt.recover(&session).unwrap();
    assert_eq!(original(&rt, &session), before);
    drop(rt);
    let rt = open(&root, provider, cfg);
    assert!(rt.pending_result_storage(&session).unwrap().is_empty());
    assert_eq!(
        rt.get(&session).unwrap().state,
        SessionState::Idle,
        "reopen state: {:#?}",
        rt.get(&session).unwrap()
    );
    assert_eq!(
        rt.prompt(&session, "explicit next task", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    assert_eq!(original(&rt, &session), before);
}

#[test]
fn directory_sync_failure_injection_cannot_escape_its_checkpoint() {
    assert!(!cayw_content::fail_current_state_directory_sync());
    let (_temp, root) = setup();
    let store = ContentStore::open(
        root.join("content"),
        Id::new("injection-scope").unwrap(),
        Limits::default(),
    )
    .unwrap();
    store
        .put(
            &Id::new("ordinary").unwrap(),
            ExpectedRevision::Absent,
            b"not affected",
        )
        .unwrap();
    assert!(!cayw_content::fail_current_state_directory_sync());
}
