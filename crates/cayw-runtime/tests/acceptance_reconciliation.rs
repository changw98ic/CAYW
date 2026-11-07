//! Acceptance reconciliation test suite.
//! Verifies cancellation, non-signaling audit, reopen reconciliation,
//! continuation, and anti-replay guarantees. Offline and deterministic.
#![cfg(target_os = "macos")]

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall,
};
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityReconciliation, CoreEvent,
    ReconciliationOrigin, ReconciliationRequest, ReconciliationVerifier, SessionState,
};
use cayw_ipc::{Client, ClientConfig, ServerConfig};
use cayw_runtime::{CancelTarget, Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use futures::stream;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

const TOKEN: &str = "acceptance-reconciliation-ipc-token";
const BUDGET: Duration = Duration::from_secs(60);
const NEXT_PROMPT: &str =
    "Begin a new task after host verification. Do not replay the old capability.";
const NEXT_REPLY: &str = "New offline task completed after trusted reconciliation.";
const ACCEPTANCE_ACTOR: &str = "acceptance-verifier-user";

fn append(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

/// Verifier that validates reconciliation against observed external process/workspace evidence.
struct AcceptanceReceiptVerifier {
    expected_call_key: Arc<Mutex<Option<cayw_core::CapabilityCallKey>>>,
    allowed_actor: String,
    verified_evidence: Arc<Mutex<String>>,
    observations: Arc<Mutex<Vec<Value>>>,
}

impl ReconciliationVerifier for AcceptanceReceiptVerifier {
    fn verify(&self, request: &ReconciliationRequest) -> std::result::Result<(), String> {
        let result = (|| -> std::result::Result<(), String> {
            let ReconciliationRequest::Capability { reconciliation } = request else {
                return Err("verifier only authorizes capability reconciliations".into());
            };
            if reconciliation.actor != self.allowed_actor {
                return Err(format!(
                    "actor '{}' is not authorized (expected '{}')",
                    reconciliation.actor, self.allowed_actor
                ));
            }
            if let Some(expected_key) = self.expected_call_key.lock().unwrap().as_ref()
                && &reconciliation.key != expected_key
            {
                return Err(format!(
                    "reconciliation key {:?} does not match expected {:?}",
                    reconciliation.key, expected_key
                ));
            }
            let expected_evidence = self.verified_evidence.lock().unwrap().clone();
            if reconciliation.evidence != expected_evidence {
                return Err("evidence does not match independently verified proof".into());
            }
            match &reconciliation.outcome {
                CapabilityOutcome::Failed { error } => {
                    if !error.contains("Cancelled") && !error.contains("cancelled") {
                        return Err("failed outcome does not record cancellation reason".into());
                    }
                }
                CapabilityOutcome::Completed { .. } => {
                    return Err(
                        "contradicts observed evidence: process was cancelled, not completed"
                            .into(),
                    );
                }
                CapabilityOutcome::NotExecuted { .. } => {
                    return Err("contradicts observed evidence: process was dispatched and produced output before cancellation".into());
                }
                _ => return Err("outcome must be a definitive known terminal".into()),
            }
            Ok(())
        })();

        let (actor, key, outcome) = match request {
            ReconciliationRequest::Capability { reconciliation } => (
                reconciliation.actor.clone(),
                serde_json::to_value(&reconciliation.key).unwrap_or(Value::Null),
                serde_json::to_value(&reconciliation.outcome).unwrap_or(Value::Null),
            ),
            ReconciliationRequest::Tool { reconciliation } => (
                reconciliation.actor.clone(),
                json!(reconciliation.call_id),
                serde_json::to_value(&reconciliation.outcome).unwrap_or(Value::Null),
            ),
        };

        self.observations.lock().unwrap().push(json!({
            "actor": actor,
            "key": key,
            "proposed_outcome": outcome,
            "accepted": result.is_ok(),
            "error": result.as_ref().err(),
        }));

        result
    }
}

/// Offline provider with distinct turns:
/// Turn 1: Invokes `test.run` with profile `cancel_probe` via Luau cell.
/// Continuation: Returns final completion text without re-invoking tools.
struct ControlledOfflineProvider {
    root: PathBuf,
    continuation_allowed: Arc<AtomicBool>,
    calls: AtomicU64,
}

impl Provider for ControlledOfflineProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let user_text = request
            .messages
            .iter()
            .rev()
            .find_map(|msg| match msg {
                Message::User { content, .. } => Some(cayw_ai::content_text(content)),
                _ => None,
            })
            .unwrap_or_default();

        let message = if user_text.contains("cancel_probe") {
            // Turn 1: Emit execute_code call invoking native test.run profile
            let mut msg = AssistantMessage::new(&request.model);
            msg.usage = cayw_ai::Usage {
                input: 100,
                output: 50,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                total_tokens: 150,
            };
            msg.usage_status = cayw_ai::UsageStatus::Reported;
            msg.content = vec![
                Content::text("Running cancel_probe via execute_code..."),
                Content::ToolCall(ToolCall::new(
                    format!("call_00_probe_{index}"),
                    "execute_code",
                    json!({
                        "action": "exec",
                        "max_output_tokens": 4096,
                        "script": "return cayw.call('test.run', { profile = 'cancel_probe' })",
                        "yield_time_ms": 1000
                    }),
                )),
            ];
            msg.stop_reason = StopReason::ToolUse;
            msg
        } else {
            // Continuation turn
            if !self.continuation_allowed.load(Ordering::SeqCst) {
                panic!("model was invoked before continuation was explicitly authorized");
            }
            let mut msg = AssistantMessage::new(&request.model);
            msg.usage = cayw_ai::Usage {
                input: 50,
                output: 20,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                total_tokens: 70,
            };
            msg.usage_status = cayw_ai::UsageStatus::Reported;
            msg.content = vec![Content::text(NEXT_REPLY)];
            msg.stop_reason = StopReason::Stop;
            msg
        };

        append(
            &self.root.join("provider-calls.jsonl"),
            &json!({"index": index, "user_text": user_text, "response": message}),
        )
        .unwrap();

        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}

fn open_runtime(
    root: &Path,
    continuation_allowed: Arc<AtomicBool>,
    verifier: Option<Arc<dyn ReconciliationVerifier>>,
) -> Result<Runtime, RuntimeError> {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    // Create authentic profile.sh script that writes its own real PID ($$) to process.pid,
    // appends to runs.log, prints output, and sleeps.
    let profile_sh = workspace.join("profile.sh");
    fs::write(
        &profile_sh,
        "#!/bin/sh\nprintf \"%s\\n\" \"$$\" > process.pid\nprintf \"%s\\n\" \"$$\" >> runs.log\nprintf \"%s\\n\" \"PROCESS_STARTED\"\nsleep 90\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&profile_sh).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&profile_sh, perms).unwrap();
    }

    let mut profiles = BTreeMap::new();
    profiles.insert(
        "cancel_probe".to_string(),
        cayw_host::Profile {
            kind: cayw_host::ProfileKind::Test,
            program: PathBuf::from("/bin/sh"),
            args: vec![profile_sh.to_string_lossy().to_string()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(90),
            max_output_bytes: 8192,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    );

    let mut config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "test.run",
            json!({"profile": "cancel_probe"}),
        )],
        max_run_time: BUDGET,
        max_queue_time: BUDGET,
        reconciliation_verifier: verifier,
        ..RuntimeConfig::default()
    };
    config.host.profiles = profiles;
    config.cells.max_yield_time = Duration::from_secs(30);
    config.codemode.timeout = BUDGET;
    config.host.call_timeout = BUDGET;

    Runtime::open(
        root.join("session.db"),
        &workspace,
        Arc::new(ControlledOfflineProvider {
            root: root.to_path_buf(),
            continuation_allowed,
            calls: AtomicU64::new(0),
        }),
        Model::new("offline-fixture", "acceptance-reconciliation"),
        config,
    )
}

struct TestServer {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    client: Client,
}

impl TestServer {
    async fn start(runtime: Runtime) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let mut config = ServerConfig::new(TOKEN);
        config.request_timeout = BUDGET;
        let task = tokio::spawn(cayw_ipc::serve(
            listener,
            Arc::new(runtime),
            config,
            stop.clone(),
        ));
        let client = Client::connect_with_config(
            address,
            TOKEN,
            ClientConfig {
                request_timeout: BUDGET,
                ..ClientConfig::default()
            },
        )
        .await?;
        Ok(Self { stop, task, client })
    }

    async fn close(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client.close().await;
        self.stop.cancel();
        self.task.await??;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TEST 1: In-Flight Cancellation -> Real PID Audit -> Reconcile -> Continue
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acceptance_cancellation_reconciliation_full_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let continuation_allowed = Arc::new(AtomicBool::new(false));
    let expected_key = Arc::new(Mutex::new(None));
    let verifier_observations = Arc::new(Mutex::new(Vec::new()));
    let verified_evidence_holder = Arc::new(Mutex::new(String::new()));

    let verifier = Arc::new(AcceptanceReceiptVerifier {
        expected_call_key: expected_key.clone(),
        allowed_actor: ACCEPTANCE_ACTOR.to_string(),
        verified_evidence: verified_evidence_holder.clone(),
        observations: verifier_observations.clone(),
    });

    let runtime = open_runtime(root, continuation_allowed.clone(), Some(verifier.clone())).unwrap();

    // 1. Create session and enqueue cancellable task
    let session = runtime
        .create(Some("acceptance_cancellation_case".into()))
        .unwrap();
    let task = runtime.enqueue(
        &session.id,
        "Use execute_code and test.run to run the configured cancel_probe profile. It intentionally waits and may be cancelled by the control client.",
    ).unwrap();
    let sid = session.id.clone();
    let tid = task.id.clone();

    // 2. Start running in background
    let rt_clone = runtime.clone();
    let tid_clone = tid.clone();
    let run_handle =
        tokio::spawn(async move { rt_clone.resume(&tid_clone, CancellationToken::new()).await });

    // Wait until process.pid is created in workspace by the actual running shell script
    let mut real_pid: Option<u32> = None;
    for _ in 0..150 {
        let pid_path = workspace.join("process.pid");
        if let Ok(text) = fs::read_to_string(&pid_path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            real_pid = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let real_pid = real_pid.expect("cancel_probe did not write process.pid within timeout");

    // Audit using safe non-signaling `ps -p <pid> -o pid=`: confirm real process is actively running
    let ps_before = std::process::Command::new("ps")
        .args(["-p", &real_pid.to_string(), "-o", "pid="])
        .output()
        .expect("ps command failed");
    assert!(
        !ps_before.stdout.is_empty(),
        "process {} must be actively running before cancel",
        real_pid
    );

    // Retrieve active task and target for cancellation
    let current_task = runtime.task(&tid).unwrap();
    let target = CancelTarget {
        session_id: sid.clone(),
        task_id: tid.clone(),
        worker_id: current_task.worker_id,
        attempt_id: current_task.attempt_id,
        fence: current_task.fence,
    };

    let cancelled = runtime.cancel(&target).unwrap();
    assert!(cancelled, "session.cancel must succeed on in-flight target");

    // Wait for the task execution to settle
    let _ = run_handle.await;

    // 3. Inspect post-cancellation state:
    // Audit that the real process has terminated via non-signaling `ps -p <pid> -o pid=`
    let ps_after = std::process::Command::new("ps")
        .args(["-p", &real_pid.to_string(), "-o", "pid="])
        .output()
        .expect("ps command failed");
    assert!(
        ps_after.stdout.is_empty(),
        "process {} must have terminated after cancellation",
        real_pid
    );

    // process.pid MUST be retained on disk (not automatically deleted!)
    assert!(
        workspace.join("process.pid").exists(),
        "process.pid must be retained on disk as authentic evidence"
    );
    let retained_pid: u32 = fs::read_to_string(workspace.join("process.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(retained_pid, real_pid);

    // Verify runs.log recorded exactly one invocation
    let runs_content = fs::read_to_string(workspace.join("runs.log")).unwrap();
    assert_eq!(runs_content.lines().count(), 1);

    // Drop original runtime instance to simulate shutdown
    drop(runtime);

    // Reopen runtime from persisted database and workspace
    let runtime = open_runtime(root, continuation_allowed.clone(), Some(verifier.clone())).unwrap();

    let post_cancel_view = runtime.get(&sid).unwrap();
    assert_eq!(
        post_cancel_view.state,
        SessionState::NeedsReconciliation,
        "reopened session with OutcomeUnknown capability must remain in NeedsReconciliation"
    );

    // Verify blind resume or blind enqueue is refused while in NeedsReconciliation
    assert!(
        runtime
            .resume(&tid, CancellationToken::new())
            .await
            .is_err(),
        "blind resume must be refused"
    );
    assert!(
        matches!(
            runtime.enqueue(&sid, "blind attempt"),
            Err(RuntimeError::NeedsReconciliation)
        ),
        "blind enqueue must fail with NeedsReconciliation"
    );

    // Locate the unresolved capability
    assert_eq!(post_cancel_view.unresolved_capabilities.len(), 1);
    let unknown_call = post_cancel_view.unresolved_capabilities[0].clone();
    assert_eq!(unknown_call.name, "test.run");
    let recorded_error = match &unknown_call.outcome {
        Some(CapabilityOutcome::OutcomeUnknown { error }) => error.clone(),
        other => panic!("expected OutcomeUnknown, got {:?}", other),
    };
    assert!(
        recorded_error.contains("Cancelled"),
        "error must record Cancelled termination: {}",
        recorded_error
    );

    *expected_key.lock().unwrap() = Some(unknown_call.key.clone());

    // Prepare independently verified evidence string
    let evidence_str = format!(
        "Verified process {} was stopped by cancellation and exited (pid_exists=false); workspace retained process.pid; outcome confirmed as Failed due to cancellation",
        real_pid
    );
    *verified_evidence_holder.lock().unwrap() = evidence_str.clone();

    // 4. Test Rejections:
    // A. Contradictory outcome (claiming Completed)
    let contradictory_req = CapabilityReconciliation {
        key: unknown_call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&unknown_call)),
        actor: ACCEPTANCE_ACTOR.to_string(),
        evidence: evidence_str.clone(),
        outcome: CapabilityOutcome::Completed {
            result: json!({"forged": true}),
        },
        external_provenance: unknown_call.external_provenance.clone(),
    };
    assert!(
        runtime
            .reconcile_capability(&sid, contradictory_req)
            .is_err(),
        "contradictory outcome must be rejected"
    );

    // B. Unauthorized actor
    let unauthorized_req = CapabilityReconciliation {
        key: unknown_call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&unknown_call)),
        actor: "untrusted-actor".to_string(),
        evidence: evidence_str.clone(),
        outcome: CapabilityOutcome::Failed {
            error: recorded_error.clone(),
        },
        external_provenance: unknown_call.external_provenance.clone(),
    };
    assert!(
        runtime
            .reconcile_capability(&sid, unauthorized_req)
            .is_err(),
        "unauthorized actor must be rejected"
    );

    // C. Mutated lease
    let mut mutated_req = CapabilityReconciliation {
        key: unknown_call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&unknown_call)),
        actor: ACCEPTANCE_ACTOR.to_string(),
        evidence: evidence_str.clone(),
        outcome: CapabilityOutcome::Failed {
            error: recorded_error.clone(),
        },
        external_provenance: unknown_call.external_provenance.clone(),
    };
    mutated_req.expected.lease.fence += 10;
    assert!(
        runtime.reconcile_capability(&sid, mutated_req).is_err(),
        "mutated lease must be rejected"
    );

    // Session remains in NeedsReconciliation
    assert_eq!(
        runtime.get(&sid).unwrap().state,
        SessionState::NeedsReconciliation
    );

    // 5. Submit valid reconciliation
    let valid_req = CapabilityReconciliation {
        key: unknown_call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&unknown_call)),
        actor: ACCEPTANCE_ACTOR.to_string(),
        evidence: evidence_str.clone(),
        outcome: CapabilityOutcome::Failed {
            error: recorded_error.clone(),
        },
        external_provenance: unknown_call.external_provenance.clone(),
    };

    let reconciled_view = runtime
        .reconcile_capability(&sid, valid_req.clone())
        .unwrap();
    assert_eq!(
        reconciled_view.state,
        SessionState::Idle,
        "reconciled session must return to Idle"
    );
    assert!(
        reconciled_view.unresolved_capabilities.is_empty(),
        "unresolved capabilities must be cleared"
    );

    // Exact retry must be idempotent
    let retry_view = runtime
        .reconcile_capability(&sid, valid_req.clone())
        .unwrap();
    assert_eq!(
        retry_view, reconciled_view,
        "exact retry must be idempotent"
    );

    // 6. Continuation: Enqueue new task and run to completion
    continuation_allowed.store(true, Ordering::SeqCst);

    let next_task = runtime.enqueue(&sid, NEXT_PROMPT).unwrap();
    assert_ne!(next_task.id, tid);

    runtime
        .resume(&next_task.id, CancellationToken::new())
        .await
        .unwrap();

    let final_view = runtime.get(&sid).unwrap();
    assert_eq!(final_view.state, SessionState::Idle);

    let has_reply = final_view.messages.iter().any(|msg| match msg {
        Message::Assistant(m) => cayw_ai::content_text(&m.content) == NEXT_REPLY,
        _ => false,
    });
    assert!(has_reply, "continuation task must deliver expected reply");

    // Strict Anti-Replay Check:
    // Confirm test.run cancel_probe was NOT executed a second time!
    let runs_after = fs::read_to_string(workspace.join("runs.log")).unwrap();
    assert_eq!(
        runs_after.lines().count(),
        1,
        "continuation task must NOT replay the cancelled capability!"
    );

    // Verify event stream contains CapabilityReconciled event
    let events = runtime.events(&sid, 0, 1000).unwrap();
    let reconciled_event_exists = events
        .iter()
        .any(|e| matches!(&e.event, CoreEvent::CapabilityReconciled { .. }));
    assert!(
        reconciled_event_exists,
        "event stream must contain CapabilityReconciled"
    );
}

// ---------------------------------------------------------------------------
// TEST 2: Authenticated IPC End-to-End Reconciliation and Continuation
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acceptance_ipc_reconciliation_and_continuation_flow() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let continuation_allowed = Arc::new(AtomicBool::new(false));
    let expected_key = Arc::new(Mutex::new(None));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let verified_evidence_holder = Arc::new(Mutex::new(String::new()));

    let verifier = Arc::new(AcceptanceReceiptVerifier {
        expected_call_key: expected_key.clone(),
        allowed_actor: "rpc-user".to_string(), // IPC replaces actor with rpc-user
        verified_evidence: verified_evidence_holder.clone(),
        observations: observations.clone(),
    });

    let runtime = open_runtime(root, continuation_allowed.clone(), Some(verifier.clone())).unwrap();

    let server = TestServer::start(runtime.clone()).await.unwrap();
    let client = &server.client;

    // 1. session.create over IPC
    let created = client
        .request(
            "session.create",
            json!({"label": "ipc_reconciliation_case"}),
        )
        .await
        .unwrap();
    let sid = created["id"].as_str().unwrap().to_string();

    // 2. task.enqueue over IPC
    let task = client
        .request(
            "task.enqueue",
            json!({
                "session_id": sid,
                "text": "Use execute_code and test.run to run cancel_probe profile",
                "images": []
            }),
        )
        .await
        .unwrap();
    let tid = task["id"].as_str().unwrap().to_string();

    // Resume in background
    let resume_client = client.clone();
    let resume_tid = tid.clone();
    let resume_task = tokio::spawn(async move {
        resume_client
            .request("task.resume", json!({"task_id": resume_tid}))
            .await
    });

    // Wait until real process writes process.pid in workspace
    let mut real_pid: Option<u32> = None;
    for _ in 0..150 {
        let pid_path = workspace.join("process.pid");
        if let Ok(text) = fs::read_to_string(&pid_path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            real_pid = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let real_pid = real_pid.expect("process.pid not written via IPC execution");

    // Audit using non-signaling `ps -p <pid> -o pid=`: confirm process is running
    let ps_before = std::process::Command::new("ps")
        .args(["-p", &real_pid.to_string(), "-o", "pid="])
        .output()
        .expect("ps command failed");
    assert!(
        !ps_before.stdout.is_empty(),
        "process {} must be running before cancel",
        real_pid
    );

    // Cancel over IPC
    let current_task = runtime.task(&tid).unwrap();
    let cancel_res = client
        .request(
            "session.cancel",
            json!({
                "session_id": sid,
                "task_id": tid,
                "worker_id": current_task.worker_id,
                "attempt_id": current_task.attempt_id,
                "fence": current_task.fence,
            }),
        )
        .await
        .unwrap();
    assert_eq!(cancel_res["cancelled"], true);

    let _ = resume_task.await;

    // Audit using non-signaling `ps -p <pid> -o pid=`: confirm process has terminated
    let ps_after = std::process::Command::new("ps")
        .args(["-p", &real_pid.to_string(), "-o", "pid="])
        .output()
        .expect("ps command failed");
    assert!(
        ps_after.stdout.is_empty(),
        "process {} must have terminated after cancel",
        real_pid
    );

    // process.pid MUST be retained on disk
    assert!(workspace.join("process.pid").exists());

    // Close initial IPC server and client to simulate process termination/restart
    drop(runtime);
    server.close().await.unwrap();

    // Reopen new IPC server and client on fresh runtime instance from persisted storage
    let reopened_runtime =
        open_runtime(root, continuation_allowed.clone(), Some(verifier.clone())).unwrap();
    let server = TestServer::start(reopened_runtime).await.unwrap();
    let client = &server.client;

    // Verify session state is NeedsReconciliation on reopened server
    let view = client
        .request("session.get", json!({"session_id": sid}))
        .await
        .unwrap();
    assert_eq!(view["state"], "needs_reconciliation");

    let unres = view["unresolved_capabilities"].as_array().unwrap();
    assert_eq!(unres.len(), 1);
    let cap_call: CapabilityCallRecord = serde_json::from_value(unres[0].clone()).unwrap();

    let recorded_error = match &cap_call.outcome {
        Some(CapabilityOutcome::OutcomeUnknown { error }) => error.clone(),
        other => panic!("expected OutcomeUnknown, got {:?}", other),
    };
    assert!(recorded_error.contains("Cancelled"));

    *expected_key.lock().unwrap() = Some(cap_call.key.clone());

    let evidence_str = format!(
        "IPC-verified exit receipt for real process {}; outcome confirmed as Failed",
        real_pid
    );
    *verified_evidence_holder.lock().unwrap() = evidence_str.clone();

    // Submit capability.reconcile over IPC
    let reconcile_req = CapabilityReconciliation {
        key: cap_call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&cap_call)),
        actor: "claimed-user".to_string(), // Server will enforce rpc-user
        evidence: evidence_str.clone(),
        outcome: CapabilityOutcome::Failed {
            error: recorded_error.clone(),
        },
        external_provenance: cap_call.external_provenance.clone(),
    };

    let recon_result = client
        .request(
            "capability.reconcile",
            json!({
                "session_id": sid,
                "reconciliation": reconcile_req,
            }),
        )
        .await
        .unwrap();

    assert_eq!(recon_result["state"], "idle");
    assert!(
        recon_result["unresolved_capabilities"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Continue with new task over IPC
    continuation_allowed.store(true, Ordering::SeqCst);
    let next_task = client
        .request(
            "task.enqueue",
            json!({
                "session_id": sid,
                "text": NEXT_PROMPT,
                "images": []
            }),
        )
        .await
        .unwrap();

    let next_tid = next_task["id"].as_str().unwrap().to_string();
    let resume_next = client
        .request("task.resume", json!({"task_id": next_tid}))
        .await
        .unwrap();
    assert_eq!(resume_next["state"], "idle");

    // Strict Anti-Replay Check over IPC
    let runs_after = fs::read_to_string(workspace.join("runs.log")).unwrap();
    assert_eq!(
        runs_after.lines().count(),
        1,
        "test.run must NOT be replayed over IPC continuation!"
    );

    server.close().await.unwrap();
}
