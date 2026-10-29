//! Verification tests for RT-01, RT-03, and WB-03 in cayw-runtime:
//! - RT-01: Preserving outcome_unknown safety, durable SQLite audit recording,
//!   and blocking tainted sessions with NeedsReconciliation.
//! - RT-03: Dynamic task fence & cancellation target resolution for follow-up actions,
//!   preserving flight on retry and verifying session ownership.
//! - WB-03: Two-phase transactional snapshot recovery in ArtifactWorkspace/Runtime,
//!   ensuring invalid action snapshots do not corrupt the artifact store.

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Model, ModelRequest,
    Provider, StopReason, Usage, UsageStatus,
};
use cayw_artifacts::{
    ActionIntent, ActionKind, ActionRecord, ActionRequest, ActionStatus, ArtifactKind,
    ArtifactReconciledOutcome, ArtifactReconciliation, ArtifactReconciliationVerifier,
    ArtifactRecord, ArtifactSnapshot, PutArtifactRequest,
};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskState};
use serde_json::json;

const WATCHDOG: Duration = Duration::from_secs(10);

#[derive(Default)]
struct DummyProvider;

impl Provider for DummyProvider {
    fn stream(&self, _request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        Box::pin(async_stream::stream! {
            cancel.cancelled().await;
            yield AssistantEvent::Done {
                message: AssistantMessage::new(&Model::new("fixture", "artifacts")),
            };
        })
    }
}

struct CompletingProvider {
    calls: Arc<AtomicUsize>,
}

impl Provider for CompletingProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut message = AssistantMessage::new(&request.model);
        message.stop_reason = StopReason::Stop;
        message
            .content
            .push(cayw_ai::Content::text("continued original session"));
        message.usage = Usage {
            input: 1,
            output: 1,
            total_tokens: 2,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        Box::pin(async_stream::stream! {
            yield AssistantEvent::Done { message };
        })
    }
}

#[derive(Default)]
struct TestReconciliationVerifier;

impl ArtifactReconciliationVerifier for TestReconciliationVerifier {
    fn verify(&self, request: &ArtifactReconciliation) -> Result<(), String> {
        if request.evidence.trim().is_empty() {
            return Err("evidence cannot be empty".into());
        }
        Ok(())
    }
}

fn open_runtime(temp: &tempfile::TempDir, permissions: Vec<ScopedPermission>) -> Runtime {
    open_runtime_with_config(temp, permissions, |_| {})
}

fn open_runtime_with_config<F>(
    temp: &tempfile::TempDir,
    permissions: Vec<ScopedPermission>,
    configure: F,
) -> Runtime
where
    F: FnOnce(&mut RuntimeConfig),
{
    std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
    let mut config = RuntimeConfig {
        max_concurrent_runs: 4,
        max_run_time: WATCHDOG,
        permissions,
        ..Default::default()
    };
    configure(&mut config);
    Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        Arc::new(DummyProvider),
        Model::new("fixture", "artifacts"),
        config,
    )
    .unwrap()
}

#[tokio::test]
async fn wb03_transactional_snapshot_recovery_preserves_store_on_action_failure() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&temp, vec![]);

    let session = runtime.create(Some("ArtifactSess".into())).unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-orig".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Original".into(),
            body: "<p>orig</p>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    let mut bundle = runtime.export_artifact_recovery();
    assert_eq!(bundle.artifacts.artifacts.len(), 1);

    // Formulate a recovery bundle with a new valid artifact and an INVALID action
    bundle.artifacts = ArtifactSnapshot {
        artifacts: vec![ArtifactRecord {
            artifact_id: "card-staged".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Staged".into(),
            body: "<p>staged</p>".into(),
            content_version: 1,
            created_at_ms: 200,
            updated_at_ms: 200,
            result_handle: None,
        }],
    };
    bundle.actions.actions.push(ActionRecord {
        action_id: "invalid action with spaces".into(),
        artifact_id: "card-staged".into(),
        session_id: session.id.clone(),
        kind: ActionKind::Button,
        intent: ActionIntent::ReadOnly { note: "bad".into() },
        status: ActionStatus::Applied,
        rejection_reason: None,
        outcome_json: None,
        requested_at_ms: 200,
        recorded_at_ms: 200,
    });

    let res = runtime.restore_artifact_recovery(bundle);
    assert!(res.is_err(), "restoration must fail validation");

    // Two-phase commit guarantee: card-orig must remain in store; card-staged must NOT be committed
    let store = runtime.artifact_store();
    assert!(
        store.get("card-orig").is_ok(),
        "original card must be preserved"
    );
    assert!(
        store.get("card-staged").is_err(),
        "staged card must not be partially applied"
    );
}

#[tokio::test]
async fn rt03_dynamic_cancellation_target_cancels_enqueued_task_and_logs_audit() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&temp, vec![]);

    let session = runtime.create(Some("FollowUpSess".into())).unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-fu".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "FollowUp Card".into(),
            body: "<button>Follow</button>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    let outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-fu-1".into(),
            artifact_id: "card-fu".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "summarize details".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let outcome_val: serde_json::Value =
        serde_json::from_str(outcome.record.outcome_json.as_deref().unwrap()).unwrap();
    let task_id = outcome_val["task_id"].as_str().unwrap();

    // Verify task is queued
    let task_record = runtime.task(task_id).unwrap();
    assert_eq!(task_record.state, TaskState::Queued);

    // Cancel action dynamically via Runtime
    let cancel_outcome = runtime
        .cancel_artifact_action(&session.id, "act-fu-1")
        .unwrap();

    assert_eq!(cancel_outcome.record.status, ActionStatus::Cancelled);

    // Verify live task has been cancelled
    let task_record = runtime.task(task_id).unwrap();
    assert_eq!(task_record.state, TaskState::Cancelled);

    // Verify audit logs
    let audit_events = runtime.audit(&session.id, 0, 100).unwrap();
    assert!(
        audit_events
            .iter()
            .any(|e| e.kind == "artifact.action.cancelled")
    );
}

#[tokio::test]
async fn rt01_tool_invoke_audit_and_unknown_outcome_blocks_session() {
    let temp = tempfile::tempdir().unwrap();
    let permission = ScopedPermission::exact("fs.read", json!({"path": "readme.txt"}));
    let runtime = open_runtime(&temp, vec![permission]);

    let session = runtime.create(Some("ToolSess".into())).unwrap();
    // Prepare file in workspace
    let file_path = runtime.workspace().join("readme.txt");
    std::fs::write(&file_path, "Hello Cayw").unwrap();

    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-tool".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Tool Card".into(),
            body: "<form></form>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    // 1. Successful preauthorized tool invocation
    let outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-tool-ok".into(),
            artifact_id: "card-tool".into(),
            session_id: session.id.clone(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.read".into(),
                args_json: json!({"path": "readme.txt"}).to_string(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);

    // Verify audit events recorded: started, settled, applied
    let audit_events = runtime.audit(&session.id, 0, 100).unwrap();
    assert!(
        audit_events
            .iter()
            .any(|e| e.kind == "artifact.tool.started")
    );
    assert!(
        audit_events
            .iter()
            .any(|e| e.kind == "artifact.tool.settled")
    );
    assert!(
        audit_events
            .iter()
            .any(|e| e.kind == "artifact.action.applied")
    );

    // 2. Unpreauthorized tool invocation is rejected without entering tool host
    let unauth_outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-tool-denied".into(),
            artifact_id: "card-tool".into(),
            session_id: session.id.clone(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.write".into(),
                args_json: json!({"path": "other.txt", "content": "bad"}).to_string(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 102,
        })
        .unwrap();

    assert_eq!(unauth_outcome.record.status, ActionStatus::Rejected);

    // 3. When an unknown effect is recorded in the session journal,
    // subsequent actions must be blocked with NeedsReconciliation.
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.action.outcome_unknown",
            1000i64,
            json!({"action_id": "act-crashed", "outcome_unknown": true}).to_string()
        ],
    )
    .unwrap();

    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // Subsequent action must fail with RuntimeError::NeedsReconciliation
    let blocked_err = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-after-unknown".into(),
            artifact_id: "card-tool".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::ReadOnly {
                note: "test".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 103,
        })
        .unwrap_err();

    assert!(matches!(blocked_err, RuntimeError::NeedsReconciliation));
}

#[tokio::test]
async fn rt01_tool_invoke_real_host_unknown_outcome_blocks_session() {
    let temp = tempfile::tempdir().unwrap();
    let permission = ScopedPermission::exact(
        "fs.write",
        json!({"path": "out.txt", "content": "hello mutating world"}),
    );
    let runtime = open_runtime_with_config(&temp, vec![permission], |cfg| {
        cfg.max_concurrent_runs = 4;
        cfg.max_run_time = WATCHDOG;
        cfg.host.max_result_bytes = 10;
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });

    let session = runtime.create(Some("ToolUnknownSess".into())).unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-mutating".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Mutating Tool Card".into(),
            body: "<form></form>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    // Invoking mutating tool fs.write with tiny max_result_bytes causes
    // ResponseBudget to return CapabilityError::unknown through real Host path.
    let outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-tool-mutate-unknown".into(),
            artifact_id: "card-mutating".into(),
            session_id: session.id.clone(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.write".into(),
                args_json: json!({"path": "out.txt", "content": "hello mutating world"})
                    .to_string(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::OutcomeUnknown);

    // Verify journal has recorded artifact.tool.settled (with outcome_unknown: true)
    // and artifact.action.outcome_unknown
    let audit_events = runtime.audit(&session.id, 0, 100).unwrap();
    assert!(audit_events.iter().any(|e| {
        e.kind == "artifact.tool.settled"
            && e.data
                .get("outcome_unknown")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
    }));
    assert!(
        audit_events
            .iter()
            .any(|e| e.kind == "artifact.action.outcome_unknown")
    );

    // Verify has_artifact_unknown detects it
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // Subsequent actions on session must be blocked with NeedsReconciliation
    let blocked_err = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-subsequent-after-unknown".into(),
            artifact_id: "card-mutating".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::ReadOnly {
                note: "test".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 102,
        })
        .unwrap_err();

    assert!(matches!(blocked_err, RuntimeError::NeedsReconciliation));
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("out.txt")).unwrap(),
        "hello mutating world"
    );
    let call_id = audit_events
        .iter()
        .find(|event| event.kind == "artifact.tool.started")
        .unwrap()
        .data["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reconciliation = ArtifactReconciliation {
        session_id: session.id.clone(),
        action_id: "act-tool-mutate-unknown".into(),
        expected_artifact_id: "card-mutating".into(),
        call_id: Some(call_id),
        actor: "trusted-test-host".into(),
        evidence: "file inspected after original Host stopped; original content matches".into(),
        outcome: ArtifactReconciledOutcome::Applied { result_json: None },
    };
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_artifact_reconciliation BEFORE INSERT ON runtime_audit
         WHEN NEW.kind = 'artifact.action.reconciled'
         BEGIN SELECT RAISE(FAIL, 'test reconciliation write failure'); END;",
    )
    .unwrap();
    assert!(
        runtime
            .reconcile_artifact_action(reconciliation.clone())
            .is_err()
    );
    assert_eq!(
        runtime
            .get_artifact_action("act-tool-mutate-unknown")
            .unwrap()
            .status,
        ActionStatus::OutcomeUnknown
    );
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());
    conn.execute_batch("DROP TRIGGER reject_artifact_reconciliation;")
        .unwrap();
    runtime.reconcile_artifact_action(reconciliation).unwrap();
    assert!(!runtime.has_artifact_unknown(&session.id).unwrap());
    let task = runtime
        .enqueue(&session.id, "continue original session")
        .unwrap();
    assert!(runtime.cancel(&task.cancel_target()).unwrap());
    assert_eq!(
        runtime
            .audit(&session.id, 0, 100)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "artifact.tool.started")
            .count(),
        1
    );
    drop(runtime);
    let reopened = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    assert!(!reopened.has_artifact_unknown(&session.id).unwrap());
    assert!(
        reopened
            .enqueue(&session.id, "continue after reopen")
            .is_ok()
    );
}

#[tokio::test]
async fn rt01_peer_reconciliation_unblocks_original_runtime_and_new_unknown_reblocks() {
    use sha2::{Digest, Sha256};

    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
    let first_arguments = json!({"path": "peer.txt", "content": "first original effect"});
    let second_arguments = json!({
        "path": "peer.txt",
        "content": "second original effect",
        "overwrite": true,
        "expected_sha256": format!("{:x}", Sha256::digest(b"first original effect")),
    });
    let mut config = RuntimeConfig {
        max_run_time: WATCHDOG,
        permissions: vec![
            ScopedPermission::exact("fs.write", first_arguments.clone()),
            ScopedPermission::exact("fs.write", second_arguments.clone()),
        ],
        artifact_reconciliation_verifier: Some(Arc::new(TestReconciliationVerifier)),
        ..Default::default()
    };
    config.host.max_result_bytes = 10;
    let model_calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        Arc::new(CompletingProvider {
            calls: model_calls.clone(),
        }),
        Model::new("fixture", "artifacts"),
        config.clone(),
    )
    .unwrap();
    let peer = Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        Arc::new(DummyProvider),
        Model::new("fixture", "artifacts"),
        config,
    )
    .unwrap();
    let session = runtime
        .create(Some("PeerReconciliationSess".into()))
        .unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-peer".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Peer reconciliation".into(),
            body: "<form></form>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();
    let first = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-peer-first".into(),
            artifact_id: "card-peer".into(),
            session_id: session.id.clone(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.write".into(),
                args_json: first_arguments.to_string(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();
    assert_eq!(first.record.status, ActionStatus::OutcomeUnknown);
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("peer.txt")).unwrap(),
        "first original effect"
    );
    assert!(matches!(
        runtime.enqueue(&session.id, "blocked before peer decision"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert!(!runtime.is_tool_flight_active("act-peer-first"));
    let audit = peer.audit(&session.id, 0, 100).unwrap();
    let call_id = audit
        .iter()
        .find(|event| event.kind == "artifact.tool.started")
        .unwrap()
        .data["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let fence = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(runtime.workspace().join(".cayw-host-read-fence"))
        .unwrap();
    fence.try_lock().unwrap();
    let decision = ArtifactReconciliation {
        session_id: session.id.clone(),
        action_id: "act-peer-first".into(),
        expected_artifact_id: "card-peer".into(),
        call_id: Some(call_id.clone()),
        actor: "trusted-test-host".into(),
        evidence:
            "original Host stopped; peer inspected peer.txt and confirmed first original effect"
                .into(),
        outcome: ArtifactReconciledOutcome::Applied { result_json: None },
    };
    let sync_peer = peer.clone();
    assert!(matches!(
        std::thread::spawn(move || sync_peer.reconcile_artifact_action(decision))
            .join()
            .expect("synchronous reconciliation must not panic without Tokio"),
        Err(RuntimeError::Busy)
    ));
    assert_eq!(
        peer.audit(&session.id, 0, 100)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "artifact.action.reconciled")
            .count(),
        1
    );
    assert!(peer.get_artifact_action("act-peer-first").is_err());
    let sync_runtime = runtime.clone();
    let sync_session_id = session.id.clone();
    assert!(matches!(
        std::thread::spawn(move || {
            sync_runtime.enqueue(&sync_session_id, "host recovery is still blocked")
        })
        .join()
        .expect("synchronous admission must not panic without Tokio"),
        Err(RuntimeError::Busy)
    ));
    let pending: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(runtime.workspace().join(".cayw-host-read-state")).unwrap(),
    )
    .unwrap();
    assert_eq!(pending["pending"]["audit_id"], call_id);
    fence.unlock().unwrap();
    let sync_peer = peer.clone();
    let sync_session_id = session.id.clone();
    assert!(
        !std::thread::spawn(move || sync_peer.has_artifact_unknown(&sync_session_id))
            .join()
            .expect("synchronous recovery must not panic without Tokio")
            .unwrap()
    );
    let recovered: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(runtime.workspace().join(".cayw-host-read-state")).unwrap(),
    )
    .unwrap();
    assert!(recovered["pending"].is_null());
    assert_eq!(recovered["generation"], pending["generation"]);
    assert_eq!(recovered["incarnation"], pending["incarnation"]);
    assert_eq!(
        runtime.get_artifact_action("act-peer-first").unwrap(),
        first.record
    );

    // A's first admission after B's decision must refresh the original record itself.
    let task = runtime
        .enqueue(&session.id, "continue original session after peer decision")
        .unwrap();
    let completed =
        tokio::time::timeout(WATCHDOG, runtime.resume(&task.id, CancellationToken::new()))
            .await
            .expect("resumed task must finish")
            .unwrap();
    assert_eq!(completed.id, session.id);
    assert_eq!(runtime.task(&task.id).unwrap().state, TaskState::Completed);
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .get_artifact_action("act-peer-first")
            .unwrap()
            .status,
        ActionStatus::Applied
    );
    let next = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-peer-readonly".into(),
            artifact_id: "card-peer".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::ReadOnly {
                note: "continue after peer reconciliation".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 102,
        })
        .unwrap();
    assert_eq!(next.record.status, ActionStatus::Applied);
    let second = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-peer-second".into(),
            artifact_id: "card-peer".into(),
            session_id: session.id.clone(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.write".into(),
                args_json: second_arguments.to_string(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 103,
        })
        .unwrap();
    assert_eq!(
        second.record.status,
        ActionStatus::OutcomeUnknown,
        "second fs.write rejection_reason={:?}; args_json_bytes={}",
        second
            .record
            .rejection_reason
            .as_deref()
            .map(|reason| reason.chars().take(512).collect::<String>()),
        second_arguments.to_string().len(),
    );
    assert!(matches!(
        runtime.enqueue(&session.id, "blocked by new effect"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert!(peer.has_artifact_unknown(&session.id).unwrap());
    assert_eq!(
        runtime.get_artifact_action("act-peer-second").unwrap(),
        second.record
    );
    assert_eq!(
        std::fs::read_to_string(runtime.workspace().join("peer.txt")).unwrap(),
        "second original effect"
    );
    let audit = runtime.audit(&session.id, 0, 100).unwrap();
    for action_id in ["act-peer-first", "act-peer-second"] {
        assert_eq!(
            audit
                .iter()
                .filter(|event| event.kind == "artifact.tool.started"
                    && event.data["action_id"] == action_id)
                .count(),
            1
        );
    }
    assert_eq!(
        audit
            .iter()
            .filter(|event| event.kind == "artifact.action.reconciled")
            .count(),
        1
    );
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rt01_peer_marker_does_not_clear_unpersisted_or_mismatched_memory_unknown() {
    for persisted_record in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let arguments = json!({"path": "local-unknown.txt", "content": "original effect"});
        let runtime = open_runtime_with_config(
            &temp,
            vec![ScopedPermission::exact("fs.write", arguments.clone())],
            |cfg| {
                cfg.host.max_result_bytes = 10;
            },
        );
        let peer = open_runtime_with_config(&temp, vec![], |cfg| {
            cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
        });
        let session = runtime
            .create(Some("UnmatchedMemoryUnknownSess".into()))
            .unwrap();
        runtime
            .put_artifact(PutArtifactRequest {
                artifact_id: "card-local-unknown".into(),
                session_id: session.id.clone(),
                kind: ArtifactKind::HtmlCard,
                title: "Local unknown".into(),
                body: "<form></form>".into(),
                at_ms: 100,
                result_handle: None,
            })
            .unwrap();
        let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
        if !persisted_record {
            conn.execute_batch(
                "CREATE TRIGGER reject_action_unknown BEFORE INSERT ON runtime_audit
                 WHEN NEW.kind = 'artifact.action.outcome_unknown'
                 BEGIN SELECT RAISE(FAIL, 'test action unknown write failure'); END;",
            )
            .unwrap();
        }
        let dispatched = runtime.dispatch_artifact_action(ActionRequest {
            action_id: "act-local-unknown".into(),
            artifact_id: "card-local-unknown".into(),
            session_id: session.id.clone(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.write".into(),
                args_json: arguments.to_string(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        });
        if persisted_record {
            assert_eq!(
                dispatched.unwrap().record.status,
                ActionStatus::OutcomeUnknown
            );
            // A later recovery record has no matching durable decision.
            let mut recovery = runtime.export_artifact_recovery();
            let local = recovery
                .actions
                .actions
                .iter_mut()
                .find(|record| record.action_id == "act-local-unknown")
                .unwrap();
            local.recorded_at_ms += 1;
            local.rejection_reason = Some("later local unknown has no durable decision".into());
            runtime.restore_artifact_recovery(recovery).unwrap();
        } else {
            assert!(matches!(dispatched, Err(RuntimeError::Journal(_))));
            conn.execute_batch("DROP TRIGGER reject_action_unknown;")
                .unwrap();
        }
        let original = runtime.get_artifact_action("act-local-unknown").unwrap();
        assert_eq!(original.status, ActionStatus::OutcomeUnknown);
        assert_eq!(
            std::fs::read_to_string(runtime.workspace().join("local-unknown.txt")).unwrap(),
            "original effect"
        );
        assert!(!runtime.is_tool_flight_active("act-local-unknown"));
        let audit = peer.audit(&session.id, 0, 100).unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|event| event.kind == "artifact.action.outcome_unknown")
                .count(),
            usize::from(persisted_record)
        );
        let call_id = audit
            .iter()
            .find(|event| event.kind == "artifact.tool.started")
            .unwrap()
            .data["call_id"]
            .as_str()
            .unwrap()
            .to_owned();
        peer.reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-local-unknown".into(),
            expected_artifact_id: "card-local-unknown".into(),
            call_id: Some(call_id),
            actor: "trusted-test-host".into(),
            evidence: "original Host stopped; peer inspected local-unknown.txt and confirmed original effect".into(),
            outcome: ArtifactReconciledOutcome::Applied { result_json: None },
        }).unwrap();
        assert!(!peer.has_artifact_unknown(&session.id).unwrap());
        assert!(matches!(
            runtime.enqueue(&session.id, "must preserve unmatched local unknown"),
            Err(RuntimeError::NeedsReconciliation)
        ));
        assert_eq!(
            runtime.get_artifact_action("act-local-unknown").unwrap(),
            original
        );
        assert!(runtime.has_artifact_unknown(&session.id).unwrap());
        assert!(matches!(
            runtime.dispatch_artifact_action(ActionRequest {
                action_id: "act-local-blocked".into(),
                artifact_id: "card-local-unknown".into(),
                session_id: session.id.clone(),
                kind: ActionKind::Button,
                intent: ActionIntent::ReadOnly {
                    note: "must remain blocked".into()
                },
                expected_content_version: Some(1),
                requested_at_ms: 102,
            }),
            Err(RuntimeError::NeedsReconciliation)
        ));
        let audit = runtime.audit(&session.id, 0, 100).unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|event| event.kind == "artifact.tool.started")
                .count(),
            1
        );
        assert_eq!(
            audit
                .iter()
                .filter(|event| event.kind == "artifact.action.reconciled")
                .count(),
            1
        );
        assert!(runtime.tasks(&session.id).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(runtime.workspace().join("local-unknown.txt")).unwrap(),
            "original effect"
        );
    }
}

#[tokio::test]
async fn rt03_dynamic_cancellation_cancels_claimed_running_task_with_advanced_fence() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&temp, vec![]);

    let session = runtime.create(Some("RunningFollowUpSess".into())).unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-running".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Running FollowUp Card".into(),
            body: "<button>Follow</button>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    let outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-running-fu".into(),
            artifact_id: "card-running".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "long running prompt".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let outcome_val: serde_json::Value =
        serde_json::from_str(outcome.record.outcome_json.as_deref().unwrap()).unwrap();
    let task_id = outcome_val["task_id"].as_str().unwrap().to_string();

    // Verify task is initially Queued with fence 0 and no worker
    let initial_task = runtime.task(&task_id).unwrap();
    assert_eq!(initial_task.state, TaskState::Queued);
    assert_eq!(initial_task.fence, 0);
    assert!(initial_task.worker_id.is_none());

    // Worker claims the task and starts execution (advancing fence, setting worker/attempt)
    let rt_clone = runtime.clone();
    let t_id = task_id.clone();
    let resume_cancel = CancellationToken::new();
    let join_handle = tokio::spawn(async move { rt_clone.resume(&t_id, resume_cancel).await });

    // Wait until the task enters Running state
    let mut claimed = false;
    for _ in 0..100 {
        let t = runtime.task(&task_id).unwrap();
        if t.state == TaskState::Running {
            assert!(t.fence > 0, "task fence must have advanced upon claiming");
            assert!(
                t.worker_id.is_some(),
                "worker must be assigned upon claiming"
            );
            claimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(claimed, "task must transition to Running state");

    // Cancel action dynamically via Runtime - RT-03 ensures live target with current fence/worker is cancelled
    let cancel_outcome = runtime
        .cancel_artifact_action(&session.id, "act-running-fu")
        .unwrap();

    assert_eq!(cancel_outcome.record.status, ActionStatus::Cancelled);

    let _ = join_handle.await;

    // Verify task is now Cancelled
    let final_task = runtime.task(&task_id).unwrap();
    assert_eq!(final_task.state, TaskState::Cancelled);

    // Verify audit logs record cancellation
    let audit_events = runtime.audit(&session.id, 0, 100).unwrap();
    assert!(
        audit_events
            .iter()
            .any(|e| e.kind == "artifact.action.cancelled")
    );
}

#[tokio::test]
async fn has_artifact_unknown_pages_through_over_1000_audit_events() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&temp, vec![]);
    let session = runtime.create(Some("PagingSess".into())).unwrap();

    // Populate SQLite runtime_audit with >1000 normal events
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute_batch("BEGIN;").unwrap();
    for i in 1..=1005 {
        conn.execute(
            "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                session.id,
                None::<String>,
                "test.normal_event",
                1000i64 + i,
                json!({"seq": i}).to_string(),
            ],
        )
        .unwrap();
    }
    // Now insert an outcome_unknown event at seq 1006 (> 1000 limit of a single page)
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.action.outcome_unknown",
            2000i64,
            json!({"action_id": "act-late-unknown", "outcome_unknown": true}).to_string(),
        ],
    )
    .unwrap();
    conn.execute_batch("COMMIT;").unwrap();

    // Verify that cursor pagination successfully reaches seq 1006 and detects unknown
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());
}

#[test]
fn art_cross_01_pre_admission_check_returns_rejected_not_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&temp, vec![]);
    let session = runtime.create(Some("PreAdmissionSess".into())).unwrap();

    // Mark session tainted with an unknown event in audit
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.action.outcome_unknown",
            1000i64,
            json!({"action_id": "act-prior-crashed", "outcome_unknown": true}).to_string()
        ],
    )
    .unwrap();

    let dispatch = runtime.artifact_host_dispatch();
    let err = dispatch
        .follow_up_task(cayw_artifacts::FollowUpDispatch {
            action_id: "act-follow-up-unadmitted",
            artifact_id: "card",
            session_id: &session.id,
            prompt: "do something",
        })
        .unwrap_err();

    assert!(
        !err.outcome_unknown,
        "pre-admission check rejection must NOT be marked outcome_unknown"
    );

    let tool_err = dispatch
        .invoke_tool(cayw_artifacts::ToolDispatch {
            action_id: "act-tool-unadmitted",
            artifact_id: "card",
            session_id: &session.id,
            tool: "fs.read",
            args_json: "{}",
        })
        .unwrap_err();

    assert!(
        !tool_err.outcome_unknown,
        "pre-admission tool check rejection must NOT be marked outcome_unknown"
    );
}

#[tokio::test]
async fn rt01_hanging_started_call_blocks_session_and_reconciles_with_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime.create(Some("HangingSess".into())).unwrap();

    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-hanging".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Hanging Card".into(),
            body: "<button>Hanging</button>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    // 1. Simulate process kill/crash: an action was accepted, tool started, but no settled/dropped event.
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute_batch("BEGIN;").unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.action.accepted",
            1001i64,
            json!({
                "action_id": "act-hanging-1",
                "artifact_id": "card-hanging",
                "session_id": session.id,
                "intent": {"type": "invoke_tool", "tool": "fs.write", "args_json": "{}"}
            }).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.tool.started",
            1002i64,
            json!({
                "action_id": "act-hanging-1",
                "call_id": "call-hang-1",
                "capability": "fs.write"
            }).to_string()
        ],
    ).unwrap();
    conn.execute_batch("COMMIT;").unwrap();

    // 2. has_artifact_unknown must detect the hanging started tool call
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // 3. Universal fence: prompt, enqueue, fork must all fail with NeedsReconciliation
    let prompt_err = runtime
        .prompt(&session.id, "hello", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(prompt_err, RuntimeError::NeedsReconciliation));

    let enqueue_err = runtime.enqueue(&session.id, "hello").unwrap_err();
    assert!(matches!(enqueue_err, RuntimeError::NeedsReconciliation));

    let fork_err = runtime.fork(&session.id, "fork-test", false).unwrap_err();
    assert!(matches!(fork_err, RuntimeError::NeedsReconciliation));

    // 4. Ordinary session recovery must NOT clear the unknown fence
    let _ = runtime.recover(&session.id);
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());
    let still_blocked = runtime.enqueue(&session.id, "after recover").unwrap_err();
    assert!(matches!(still_blocked, RuntimeError::NeedsReconciliation));

    // 5. Invalid reconciliation requests must be rejected
    // - absence of verifier strictly denies reconciliation
    let no_v_runtime = open_runtime(&temp, vec![]);
    let no_v_err = no_v_runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-hanging-1".into(),
            expected_artifact_id: "card-hanging".into(),
            call_id: Some("call-hang-1".into()),
            actor: "admin".into(),
            evidence: "verified external logs".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "no-op".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(no_v_err, RuntimeError::InvalidInput(_)));
    assert!(
        no_v_err
            .to_string()
            .contains("artifact_reconciliation_verifier")
    );

    // - empty evidence
    let bad_evidence = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-hanging-1".into(),
            expected_artifact_id: "card-hanging".into(),
            call_id: Some("call-hang-1".into()),
            actor: "admin".into(),
            evidence: "   ".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "no-op".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(bad_evidence, RuntimeError::InvalidInput(_)));

    // - mismatched artifact ID
    let bad_artifact = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-hanging-1".into(),
            expected_artifact_id: "wrong-card".into(),
            call_id: Some("call-hang-1".into()),
            actor: "admin".into(),
            evidence: "verified external logs".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "no-op".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(bad_artifact, RuntimeError::InvalidInput(_)));

    // 6. Trusted reconciliation with verified external evidence succeeds
    let reconciled_record = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-hanging-1".into(),
            expected_artifact_id: "card-hanging".into(),
            call_id: Some("call-hang-1".into()),
            actor: "admin@corp.internal".into(),
            evidence: "audit logs show process crashed before syscall invocation".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "crashed prior to invocation".into(),
            },
        })
        .unwrap();
    assert_eq!(reconciled_record.status, ActionStatus::Cancelled);

    // 7. Unknown fence is now cleared
    assert!(!runtime.has_artifact_unknown(&session.id).unwrap());

    // 8. Normal operations (enqueue) now succeed
    let task = runtime
        .enqueue(&session.id, "post reconciliation task")
        .unwrap();
    assert_eq!(task.state, TaskState::Queued);

    // 9. Persistence restart: dropping runtime and reopening from the same database
    // maintains the reconciled state and keeps the session unblocked
    drop(runtime);
    let reopened = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    assert!(!reopened.has_artifact_unknown(&session.id).unwrap());
    let persisted = reopened.task(&task.id).unwrap();
    assert_eq!(persisted.state, TaskState::Queued);
    assert!(reopened.cancel(&persisted.cancel_target()).unwrap());
    let reopened_task = reopened.enqueue(&session.id, "after restart task").unwrap();
    assert_eq!(reopened_task.state, TaskState::Queued);
}

#[tokio::test]
async fn rt01_active_flight_cannot_be_reconciled_while_running() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });

    let session = runtime.create(Some("ActiveFollowUpSess".into())).unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-active-fu".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Active FollowUp Card".into(),
            body: "<button>Follow</button>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    let outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-active-fu".into(),
            artifact_id: "card-active-fu".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "active prompt".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);

    // While follow-up task is active, flight is tracked in memory
    assert!(runtime.is_tool_flight_active("act-active-fu"));

    // Attempting to reconcile an active flight must return RuntimeError::Busy
    let busy_err = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-active-fu".into(),
            expected_artifact_id: "card-active-fu".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "attempt while busy".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(busy_err, RuntimeError::Busy));

    // Cancel the action to terminate flight
    let cancel = runtime
        .cancel_artifact_action(&session.id, "act-active-fu")
        .unwrap();
    assert_eq!(cancel.record.status, ActionStatus::Cancelled);
    assert!(!runtime.is_tool_flight_active("act-active-fu"));
}

struct CustomReconciliationVerifier;

impl ArtifactReconciliationVerifier for CustomReconciliationVerifier {
    fn verify(&self, request: &ArtifactReconciliation) -> Result<(), String> {
        if !request.evidence.starts_with("VERIFIED_BY_OP:") {
            return Err("evidence must start with VERIFIED_BY_OP:".into());
        }
        Ok(())
    }
}

#[tokio::test]
async fn rt01_custom_verifier_enforces_domain_evidence_rules() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(CustomReconciliationVerifier));
    });

    let session = runtime.create(Some("VerifierSess".into())).unwrap();
    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-v".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "Verifier Card".into(),
            body: "<p>V</p>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    // Mark session tainted with an unknown event
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.action.outcome_unknown",
            1000i64,
            json!({"action_id": "act-v-1", "artifact_id": "card-v", "outcome_unknown": true}).to_string()
        ],
    ).unwrap();

    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // 1. Reconcile with evidence that does NOT satisfy CustomReconciliationVerifier
    let rejected_err = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-v-1".into(),
            expected_artifact_id: "card-v".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "some unverified text".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();

    assert!(matches!(rejected_err, RuntimeError::InvalidInput(_)));
    assert!(rejected_err.to_string().contains("VERIFIED_BY_OP:"));
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // 2. Reconcile with valid evidence satisfying CustomReconciliationVerifier
    let ok_record = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-v-1".into(),
            expected_artifact_id: "card-v".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "VERIFIED_BY_OP: verified system logs and confirmed no side effects".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "verified no effect".into(),
            },
        })
        .unwrap();

    assert_eq!(ok_record.status, ActionStatus::Cancelled);
    assert!(!runtime.has_artifact_unknown(&session.id).unwrap());
    assert!(runtime.enqueue(&session.id, "post verifier task").is_ok());
}

#[tokio::test]
async fn rt01_reconcile_without_configured_verifier_is_strictly_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&temp, vec![]);
    let session = runtime.create(Some("NoVerifierSess".into())).unwrap();

    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            None::<String>,
            "artifact.action.outcome_unknown",
            1000i64,
            json!({"action_id": "act-no-v", "artifact_id": "card-no-v", "outcome_unknown": true}).to_string()
        ],
    ).unwrap();

    let err = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-no-v".into(),
            expected_artifact_id: "card-no-v".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "valid looking evidence".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();

    assert!(matches!(err, RuntimeError::InvalidInput(_)));
    assert!(err.to_string().contains("artifact_reconciliation_verifier"));
}

#[tokio::test]
async fn rt01_multi_call_reconciliation_only_clears_matching_call_id() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime.create(Some("MultiCallSess".into())).unwrap();

    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute_batch("BEGIN;").unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.accepted', 1000, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-multi", "artifact_id": "card-m"}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.tool.started', 1001, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-multi", "artifact_id": "card-m", "call_id": "call-1"}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.tool.dropped', 1002, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-multi", "artifact_id": "card-m", "call_id": "call-1", "outcome_unknown": true}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.tool.started', 1003, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-multi", "artifact_id": "card-m", "call_id": "call-2"}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.tool.dropped', 1004, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-multi", "artifact_id": "card-m", "call_id": "call-2", "outcome_unknown": true}).to_string()
        ],
    ).unwrap();
    conn.execute_batch("COMMIT;").unwrap();

    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // 1. Reconcile ONLY call-1
    runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-multi".into(),
            expected_artifact_id: "card-m".into(),
            call_id: Some("call-1".into()),
            actor: "admin".into(),
            evidence: "reconciled call 1 only".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "cleared call 1".into(),
            },
        })
        .unwrap();

    // Session MUST still be blocked because call-2 is unreconciled
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());
    assert!(matches!(
        runtime.enqueue(&session.id, "try").unwrap_err(),
        RuntimeError::NeedsReconciliation
    ));

    // 2. Reconcile call-2
    runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-multi".into(),
            expected_artifact_id: "card-m".into(),
            call_id: Some("call-2".into()),
            actor: "admin".into(),
            evidence: "reconciled call 2".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "cleared call 2".into(),
            },
        })
        .unwrap();

    // Now all calls are reconciled, session unblocks
    assert!(!runtime.has_artifact_unknown(&session.id).unwrap());
    assert!(runtime.enqueue(&session.id, "post unblock").is_ok());
}

#[tokio::test]
async fn rt01_subsequent_new_unknown_reblocks_session_after_prior_reconciliation() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime
        .create(Some("SubsequentUnknownSess".into()))
        .unwrap();

    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.outcome_unknown', 1000, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-first", "artifact_id": "card-sub"}).to_string()
        ],
    ).unwrap();

    assert!(runtime.has_artifact_unknown(&session.id).unwrap());

    // Reconcile act-first
    runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-first".into(),
            expected_artifact_id: "card-sub".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "cleared first".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "cleared".into(),
            },
        })
        .unwrap();

    assert!(!runtime.has_artifact_unknown(&session.id).unwrap());

    // Now record a SUBSEQUENT new unknown event with a higher cursor/seq
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.outcome_unknown', 2000, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-second", "artifact_id": "card-sub"}).to_string()
        ],
    ).unwrap();

    // The prior reconciliation marker for act-first must NOT swallow the new unknown for act-second
    assert!(runtime.has_artifact_unknown(&session.id).unwrap());
    assert!(matches!(
        runtime.enqueue(&session.id, "try").unwrap_err(),
        RuntimeError::NeedsReconciliation
    ));
}

#[tokio::test]
async fn rt01_restart_not_found_validates_settled_idempotency_and_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime.create(Some("RestartNotFoundSess".into())).unwrap();

    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute_batch("BEGIN;").unwrap();
    // 1. Normal action: started + settled(success: true, outcome_unknown: false) + applied
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.accepted', 1000, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-normal-1", "artifact_id": "card-norm"}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.tool.started', 1001, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-normal-1", "artifact_id": "card-norm", "call_id": "call-norm-1"}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.tool.settled', 1002, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-normal-1", "artifact_id": "card-norm", "call_id": "call-norm-1", "success": true, "outcome_unknown": false}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.applied', 1003, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-normal-1", "artifact_id": "card-norm"}).to_string()
        ],
    ).unwrap();

    // 2. Action with unknown outcome
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.accepted', 1004, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-crash-1", "artifact_id": "card-norm"}).to_string()
        ],
    ).unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.outcome_unknown', 1005, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-crash-1", "artifact_id": "card-norm", "outcome_unknown": true}).to_string()
        ],
    ).unwrap();
    conn.execute_batch("COMMIT;").unwrap();

    // Calling reconcile on act-normal-1 must be rejected because it settled normally without unknown
    let normal_err = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-normal-1".into(),
            expected_artifact_id: "card-norm".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "trying to reconcile normal action".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "bad".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(normal_err, RuntimeError::InvalidInput(_)));
    assert!(
        normal_err
            .to_string()
            .contains("no recorded unknown effect")
    );

    // First reconciliation of act-crash-1 succeeds
    let rec1 = runtime
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-crash-1".into(),
            expected_artifact_id: "card-norm".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "initial reconciliation".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "confirmed aborted".into(),
            },
        })
        .unwrap();
    assert_eq!(rec1.status, ActionStatus::Cancelled);

    // Drop and reopen runtime to simulate restart with empty in-memory actions (forcing NotFound branch)
    drop(runtime);
    let reopened = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });

    // Idempotent retry with identical outcome must succeed
    let idempotent_rec = reopened
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-crash-1".into(),
            expected_artifact_id: "card-norm".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "idempotent retry".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "confirmed aborted".into(),
            },
        })
        .unwrap();
    assert_eq!(idempotent_rec.status, ActionStatus::Cancelled);

    // Conflicting decision on historical reconciliation must be rejected
    let conflict_err = reopened
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-crash-1".into(),
            expected_artifact_id: "card-norm".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "conflicting decision".into(),
            outcome: ArtifactReconciledOutcome::Applied { result_json: None },
        })
        .unwrap_err();
    assert!(matches!(conflict_err, RuntimeError::InvalidInput(_)));
    assert!(
        conflict_err
            .to_string()
            .contains("conflicting reconciliation decision")
    );
}

#[tokio::test]
async fn rt03_follow_up_normal_completion_clears_flight_and_avoids_permanent_busy() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime.create(Some("FollowUpFlightSess".into())).unwrap();

    runtime
        .put_artifact(PutArtifactRequest {
            artifact_id: "card-fu-flight".into(),
            session_id: session.id.clone(),
            kind: ArtifactKind::HtmlCard,
            title: "FU Card".into(),
            body: "<button>FU</button>".into(),
            at_ms: 100,
            result_handle: None,
        })
        .unwrap();

    let outcome = runtime
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-fu-done".into(),
            artifact_id: "card-fu-flight".into(),
            session_id: session.id.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "summarize something".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 101,
        })
        .unwrap();

    let outcome_val: serde_json::Value =
        serde_json::from_str(outcome.record.outcome_json.as_deref().unwrap()).unwrap();
    let task_id = outcome_val["task_id"].as_str().unwrap().to_string();

    // Flight is active while task is Queued
    assert!(runtime.is_tool_flight_active("act-fu-done"));

    // Simulate normal task completion in SQLite (using authentic runtime_tasks table)
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute(
        "UPDATE runtime_tasks SET state = 'completed' WHERE id = ?1",
        [&task_id],
    )
    .unwrap();

    // Upon checking, normal terminal state removes flight and returns false
    assert!(!runtime.is_tool_flight_active("act-fu-done"));

    // Verify it is permanently removed from flights (not leaking or causing Busy)
    assert!(!runtime.is_tool_flight_active("act-fu-done"));
}

#[tokio::test]
async fn rt01_cross_runtime_active_executor_locks_block_reconciliation() {
    let temp = tempfile::tempdir().unwrap();
    let runtime1 = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime1.create(Some("CrossRuntimeSess".into())).unwrap();
    let cross_task = runtime1
        .enqueue(&session.id, "cross runtime prompt")
        .unwrap();

    // Insert action unknown in the shared database audit journal
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    conn.execute_batch("BEGIN;").unwrap();
    conn.execute(
        "INSERT INTO runtime_audit(session, task, kind, timestamp, payload) VALUES (?1, ?2, 'artifact.action.outcome_unknown', 1000, ?3)",
        rusqlite::params![
            session.id,
            None::<String>,
            json!({"action_id": "act-cross", "artifact_id": "card-cross", "outcome_unknown": true}).to_string()
        ],
    ).unwrap();
    conn.execute_batch("COMMIT;").unwrap();

    // Open Runtime 2 pointing to the same database and workspace
    let runtime2 = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });

    // 1. While task is in Queued state, cross-runtime reconciliation is blocked with Busy
    let busy_err_queued = runtime2
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-cross".into(),
            expected_artifact_id: "card-cross".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "cross-runtime attempt while queued".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(busy_err_queued, RuntimeError::Busy));

    // 2. While task is in Running state, cross-runtime reconciliation is blocked with Busy
    conn.execute(
        "UPDATE runtime_tasks SET state = 'running' WHERE id = ?1",
        [&cross_task.id],
    )
    .unwrap();
    let busy_err_running = runtime2
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-cross".into(),
            expected_artifact_id: "card-cross".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "cross-runtime attempt while running".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(busy_err_running, RuntimeError::Busy));

    // 3. While task is in WaitingApproval state, cross-runtime reconciliation is also blocked with Busy
    conn.execute(
        "UPDATE runtime_tasks SET state = 'waiting_approval' WHERE id = ?1",
        [&cross_task.id],
    )
    .unwrap();
    let busy_err_waiting = runtime2
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-cross".into(),
            expected_artifact_id: "card-cross".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "cross-runtime attempt while waiting approval".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(busy_err_waiting, RuntimeError::Busy));

    // 4. When task is marked completed in runtime_tasks but an active executor still holds
    // the coordination task lock (.runtime-lock.task-{id}), cross-runtime reconciliation
    // must still be blocked with Busy.
    conn.execute(
        "UPDATE runtime_tasks SET state = 'completed' WHERE id = ?1",
        [&cross_task.id],
    )
    .unwrap();

    let mut lock_path = temp
        .path()
        .join("sessions.db")
        .canonicalize()
        .unwrap()
        .into_os_string();
    lock_path.push(format!(".runtime-lock.task-{}", cross_task.id));
    let task_lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(std::path::PathBuf::from(lock_path))
        .unwrap();
    task_lock_file.lock().unwrap();

    let busy_err_lock = runtime2
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-cross".into(),
            expected_artifact_id: "card-cross".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "cross-runtime attempt while task coordination lock held".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "test".into(),
            },
        })
        .unwrap_err();
    assert!(matches!(busy_err_lock, RuntimeError::Busy));

    // 5. Release the coordination file lock; now Runtime 2 can successfully reconcile the action
    drop(task_lock_file);

    let rec = runtime2
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-cross".into(),
            expected_artifact_id: "card-cross".into(),
            call_id: None,
            actor: "admin".into(),
            evidence: "cross-runtime attempt after execution stopped and lock released".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "stopped".into(),
            },
        })
        .unwrap();
    assert_eq!(rec.status, ActionStatus::Cancelled);
}

struct BlockingArtifactHost {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    release: Condvar,
    calls: AtomicUsize,
}

impl BlockingArtifactHost {
    fn new() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

#[async_trait::async_trait]
impl cayw_host::ExternalHost for BlockingArtifactHost {
    async fn call(
        &self,
        name: &str,
        _arguments: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, cayw_codemode::CapabilityError> {
        assert_eq!(name, "mcp.discover");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let (_released, timeout) = self
            .release
            .wait_timeout_while(self.released.lock().unwrap(), WATCHDOG, |released| {
                !*released
            })
            .unwrap();
        if timeout.timed_out() {
            return Err(cayw_codemode::CapabilityError::unknown(
                "test release watchdog expired",
            ));
        }
        if cancel.is_cancelled() {
            Err(cayw_codemode::CapabilityError::unknown(
                "original service stopped after cancellation",
            ))
        } else {
            Ok(json!({"tools": []}))
        }
    }
}

struct ReleaseArtifactHost(Arc<BlockingArtifactHost>);
impl Drop for ReleaseArtifactHost {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rt01_direct_host_executor_blocks_peer_runtime_until_real_stop() {
    for cancelled in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let service = Arc::new(BlockingArtifactHost::new());
        let _release_on_failure = ReleaseArtifactHost(service.clone());
        let open = || {
            open_runtime_with_config(
                &temp,
                vec![ScopedPermission::exact("mcp.discover", json!({}))],
                |cfg| {
                    cfg.host.external = Some(service.clone());
                    cfg.artifact_reconciliation_verifier =
                        Some(Arc::new(TestReconciliationVerifier));
                },
            )
        };
        let runtime = open();
        let peer = open();
        let session = runtime.create(None).unwrap();
        runtime
            .put_artifact(PutArtifactRequest {
                artifact_id: "card-live".into(),
                session_id: session.id.clone(),
                kind: ArtifactKind::HtmlCard,
                title: "Live".into(),
                body: "<button>Run</button>".into(),
                at_ms: 1,
                result_handle: None,
            })
            .unwrap();
        let running = runtime.clone();
        let session_id = session.id.clone();
        let worker = tokio::task::spawn_blocking(move || {
            running.dispatch_artifact_action(ActionRequest {
                action_id: "act-live".into(),
                artifact_id: "card-live".into(),
                session_id,
                kind: ActionKind::Button,
                intent: ActionIntent::InvokeTool {
                    tool: "mcp.discover".into(),
                    args_json: "{}".into(),
                },
                expected_content_version: Some(1),
                requested_at_ms: 2,
            })
        });
        tokio::time::timeout(WATCHDOG, service.entered.notified())
            .await
            .unwrap();
        let events = peer.audit(&session.id, 0, 100).unwrap();
        let started = events
            .iter()
            .find(|event| event.kind == "artifact.tool.started")
            .unwrap();
        assert_eq!(started.data["artifact_id"], "card-live");
        let call_id = started.data["call_id"].as_str().unwrap().to_owned();
        assert!(!peer.has_artifact_unknown(&session.id).unwrap());
        let reconciliation = ArtifactReconciliation {
            session_id: session.id.clone(),
            action_id: "act-live".into(),
            expected_artifact_id: "card-live".into(),
            call_id: Some(call_id),
            actor: "trusted-test-host".into(),
            evidence: "original service stop checked".into(),
            outcome: ArtifactReconciledOutcome::NotExecuted {
                reason: "no external effects".into(),
            },
        };
        assert!(matches!(
            peer.enqueue(&session.id, "peer prompt"),
            Err(RuntimeError::Busy)
        ));
        assert!(matches!(
            peer.fork(&session.id, "peer fork", false),
            Err(RuntimeError::Busy)
        ));
        assert!(matches!(
            peer.reconcile_artifact_action(reconciliation.clone()),
            Err(RuntimeError::Busy)
        ));
        if cancelled {
            runtime
                .cancel_artifact_action(&session.id, "act-live")
                .unwrap();
            assert!(runtime.is_tool_flight_active("act-live"));
            assert!(matches!(
                peer.enqueue(&session.id, "after cancel signal"),
                Err(RuntimeError::Busy)
            ));
            assert!(matches!(
                peer.reconcile_artifact_action(reconciliation.clone()),
                Err(RuntimeError::Busy)
            ));
        }
        service.release();
        let outcome = tokio::time::timeout(WATCHDOG, worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!runtime.is_tool_flight_active("act-live"));
        assert_eq!(service.calls.load(Ordering::SeqCst), 1);
        if cancelled {
            assert_eq!(outcome.record.status, ActionStatus::OutcomeUnknown);
            assert!(peer.has_artifact_unknown(&session.id).unwrap());
            assert!(matches!(
                peer.enqueue(&session.id, "before evidence"),
                Err(RuntimeError::NeedsReconciliation)
            ));
            runtime.reconcile_artifact_action(reconciliation).unwrap();
            assert!(!peer.has_artifact_unknown(&session.id).unwrap());
        } else {
            assert_eq!(outcome.record.status, ActionStatus::Applied);
        }
        let task = peer.enqueue(&session.id, "after original stopped").unwrap();
        assert!(peer.cancel(&task.cancel_target()).unwrap());
        peer.fork(&session.id, "after stop fork", true).unwrap();
        assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rt01_same_action_call_reused_after_reconciliation_requires_new_facts() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = open_runtime_with_config(&temp, vec![], |cfg| {
        cfg.artifact_reconciliation_verifier = Some(Arc::new(TestReconciliationVerifier));
    });
    let session = runtime.create(None).unwrap();
    let conn = rusqlite::Connection::open(temp.path().join("sessions.db")).unwrap();
    let reconciliation = ArtifactReconciliation {
        session_id: session.id.clone(),
        action_id: "act-reused".into(),
        expected_artifact_id: "card-reused".into(),
        call_id: Some("call-reused".into()),
        actor: "trusted-test-host".into(),
        evidence: "exact original execution inspected".into(),
        outcome: ArtifactReconciledOutcome::NotExecuted {
            reason: "checked".into(),
        },
    };
    for round in 0..2 {
        conn.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,NULL,'artifact.tool.started',1,?2)",
            rusqlite::params![session.id, json!({"action_id":"act-reused","artifact_id":"card-reused","call_id":"call-reused"}).to_string()],
        ).unwrap();
        conn.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,NULL,'artifact.tool.dropped',2,?2)",
            rusqlite::params![session.id, json!({"action_id":"act-reused","artifact_id":"card-reused","call_id":"call-reused","outcome_unknown":true}).to_string()],
        ).unwrap();
        assert!(runtime.has_artifact_unknown(&session.id).unwrap());
        runtime
            .reconcile_artifact_action(reconciliation.clone())
            .unwrap();
        assert!(!runtime.has_artifact_unknown(&session.id).unwrap());
        let decisions: Vec<_> = runtime
            .audit(&session.id, 0, 100)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "artifact.action.reconciled")
            .collect();
        assert_eq!(decisions.len(), round + 1);
        if round == 1 {
            assert_ne!(decisions[0].data["facts"], decisions[1].data["facts"]);
        }
    }
}
