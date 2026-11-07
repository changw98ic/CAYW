//! F09/F19 audit remediation: artifact actions reach Runtime/Host (not Recording-only).

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_ipc::RpcHandler;
use cayw_runtime::{
    ActionIntent, ActionKind, ActionRequest, ActionStatus, ArtifactKind, ArtifactReconciledOutcome,
    ArtifactReconciliation, ArtifactReconciliationVerifier, PutArtifactRequest, Runtime,
    RuntimeConfig, RuntimeError, RuntimeHostDispatch, ScopedPermission, TaskState,
};
use futures::stream;
use serde_json::json;
use std::sync::Arc;

struct IdleProvider;
impl Provider for IdleProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(&request, "return 1", "idle-1");
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}

fn open(permissions: Vec<ScopedPermission>) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(IdleProvider),
        Model::new("fixture", "artifact-dispatch"),
        RuntimeConfig {
            permissions,
            ..Default::default()
        },
    )
    .unwrap();
    (dir, rt)
}

fn put_card(rt: &Runtime, session: &str, id: &str, body: &str) {
    rt.put_artifact(PutArtifactRequest {
        artifact_id: id.into(),
        session_id: session.into(),
        kind: ArtifactKind::HtmlCard,
        title: format!("Card {id}"),
        body: body.into(),
        at_ms: 1,
        result_handle: None,
    })
    .unwrap();
}

#[test]
fn product_default_dispatch_is_runtime_host_dispatch() {
    let (_dir, rt) = open(vec![]);
    let dispatch = rt.artifact_host_dispatch();
    assert!(
        dispatch
            .as_ref()
            .as_any()
            .downcast_ref::<RuntimeHostDispatch>()
            .is_some(),
        "Runtime product path must default to RuntimeHostDispatch"
    );
}

#[test]
fn follow_up_action_enqueues_runtime_task() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(Some("artifacts".into())).unwrap().id;
    put_card(&rt, &session, "card-fu", "<button>retry</button>");

    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-fu".into(),
            artifact_id: "card-fu".into(),
            session_id: session.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "retry the summary from the card".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let json = outcome.record.outcome_json.as_deref().unwrap();
    assert!(json.contains("follow_up_task_dispatched"));
    assert!(json.contains("task_id"));
    assert!(!json.contains("pending_follow_up_task"));

    let tasks = rt.tasks(&session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Queued);
    assert_eq!(tasks[0].input.text, "retry the summary from the card");
    assert!(json.contains(&tasks[0].id));
}

#[test]
fn invoke_tool_action_calls_real_host() {
    let (dir, rt) = open(vec![ScopedPermission::exact(
        "fs.read",
        json!({"path": "note.txt"}),
    )]);
    std::fs::write(dir.path().join("workspace/note.txt"), "host-evidence").unwrap();
    let session = rt.create(Some("tools".into())).unwrap().id;
    put_card(&rt, &session, "card-tool", "<form></form>");

    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-tool".into(),
            artifact_id: "card-tool".into(),
            session_id: session,
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.read".into(),
                args_json: "{\"path\":\"note.txt\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 12,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let json = outcome.record.outcome_json.unwrap();
    assert!(json.contains("tool_invoked"));
    assert!(json.contains("fs.read"));
    assert!(
        json.contains("host-evidence"),
        "Host fs.read result must appear in outcome: {json}"
    );
    assert!(!json.contains("pending_tool_invoke"));
}

#[test]
fn invoke_tool_without_permission_is_rejected() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(None).unwrap().id;
    put_card(&rt, &session, "card-deny", "<button>x</button>");
    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-deny".into(),
            artifact_id: "card-deny".into(),
            session_id: session,
            kind: ActionKind::Button,
            intent: ActionIntent::InvokeTool {
                tool: "fs.read".into(),
                args_json: "{\"path\":\"secret.txt\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 1,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Rejected);
    assert!(
        outcome
            .record
            .rejection_reason
            .as_deref()
            .unwrap()
            .contains("not preauthorized")
    );
}

#[test]
fn recovery_round_trip_keeps_runtime_dispatch() {
    let (dir, rt) = open(vec![]);
    let session = rt.create(None).unwrap().id;
    put_card(&rt, &session, "card-disk", "<p>disk</p>");
    rt.dispatch_artifact_action(ActionRequest {
        action_id: "act-state".into(),
        artifact_id: "card-disk".into(),
        session_id: session,
        kind: ActionKind::Button,
        intent: ActionIntent::UpdateState {
            key: "tab".into(),
            value_json: "\"b\"".into(),
        },
        expected_content_version: Some(1),
        requested_at_ms: 2,
    })
    .unwrap();

    let path = rt.save_artifact_recovery().unwrap();
    // Runtime::open canonicalizes the workspace root; compare that way so macOS
    // /var vs /private/var does not break the assertion (Linux is a no-op).
    assert_eq!(
        path.canonicalize().unwrap(),
        dir.path()
            .join("workspace/artifacts/recovery.json")
            .canonicalize()
            .unwrap()
    );

    // Peer Runtime on the same workspace loads recovery with RuntimeHostDispatch.
    let peer = Runtime::open(
        dir.path().join("peer.db"),
        dir.path().join("workspace"),
        Arc::new(IdleProvider),
        Model::new("fixture", "artifact-dispatch-peer"),
        RuntimeConfig::default(),
    )
    .unwrap();
    peer.load_artifact_recovery().unwrap();
    assert_eq!(
        peer.preview_artifact("card-disk").unwrap().html,
        "<p>disk</p>"
    );
    assert_eq!(
        peer.artifact_actions()
            .artifact_state("card-disk")
            .unwrap()
            .get("tab"),
        Some(&"\"b\"".to_string())
    );

    let peer_session = peer.create(Some("peer".into())).unwrap().id;
    peer.put_artifact(PutArtifactRequest {
        artifact_id: "card-peer".into(),
        session_id: peer_session.clone(),
        kind: ArtifactKind::HtmlCard,
        title: "Peer".into(),
        body: "<button>go</button>".into(),
        at_ms: 50,
        result_handle: None,
    })
    .unwrap();
    let outcome = peer
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-after".into(),
            artifact_id: "card-peer".into(),
            session_id: peer_session.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "after recovery".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 99,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let tasks = peer.tasks(&peer_session).unwrap();
    assert!(
        tasks.iter().any(|t| t.input.text == "after recovery"),
        "loaded recovery must keep RuntimeHostDispatch as product path"
    );
}

#[test]
fn follow_up_action_cancel_cancels_queued_task_and_audits() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(Some("cancel".into())).unwrap().id;
    put_card(&rt, &session, "card-cancel", "<button>go</button>");

    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-cancel".into(),
            artifact_id: "card-cancel".into(),
            session_id: session.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "work from card".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 20,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let task_id = rt.tasks(&session).unwrap()[0].id.clone();

    let cancelled = rt.cancel_artifact_action(&session, "act-cancel").unwrap();
    assert_eq!(cancelled.record.status, ActionStatus::Cancelled);

    let task = rt.task(&task_id).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);

    let listed = rt.list_artifact_actions(&session).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, ActionStatus::Cancelled);

    let audit = rt.audit(&session, 0, 100).unwrap();
    let kinds: Vec<_> = audit.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"artifact.action.applied"),
        "expected applied audit row, got {kinds:?}"
    );
    assert!(
        kinds.contains(&"artifact.action.cancelled"),
        "expected cancelled audit row, got {kinds:?}"
    );
}

#[test]
fn rejected_action_still_writes_audit_trail() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(None).unwrap().id;
    put_card(&rt, &session, "card-rej", "<button>x</button>");
    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-rej".into(),
            artifact_id: "card-rej".into(),
            session_id: session.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::InvokeTool {
                tool: "fs.read".into(),
                args_json: "{\"path\":\"missing.txt\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 1,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Rejected);
    let audit = rt.audit(&session, 0, 100).unwrap();
    assert!(
        audit.iter().any(|e| e.kind == "artifact.action.rejected"
            && e.data.get("action_id").and_then(|v| v.as_str()) == Some("act-rej")),
        "rejected dispatch must appear in unified Runtime audit"
    );
}

async fn rpc(rt: &Runtime, method: &str, params: serde_json::Value) -> serde_json::Value {
    RpcHandler::call(rt, method, params, CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn canonical_artifacts_rpc_reaches_runtime_and_preserves_session_ownership() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(Some("rpc-artifacts".into())).unwrap().id;
    let foreign = rt.create(Some("other-session".into())).unwrap().id;
    let created = rpc(
        &rt,
        "artifacts.put",
        json!({
            "artifact_id":"rpc-card", "session_id":session, "kind":"html_card",
            "title":"Original", "body":"<p>original</p>", "at_ms":1
        }),
    )
    .await;
    assert_eq!(created["content_version"], 1);
    let updated = rpc(
        &rt,
        "artifacts.update",
        json!({
            "artifact_id":"rpc-card", "session_id":session, "title":"Reviewed",
            "body":"<p>reviewed</p>", "at_ms":2, "expected_content_version":1
        }),
    )
    .await;
    assert_eq!(updated["content_version"], 2);
    let listed = rpc(&rt, "artifacts.list", json!({"session_id":session})).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let preview = rpc(&rt, "artifacts.preview", json!({"artifact_id":"rpc-card"})).await;
    assert_eq!(preview["html"], "<p>reviewed</p>");
    assert_eq!(preview["content_version"], 2);

    let action = rpc(
        &rt,
        "artifacts.action",
        json!({
            "action_id":"rpc-follow-up", "artifact_id":"rpc-card", "session_id":session,
            "kind":"button", "intent":{"type":"follow_up_task","prompt":"Follow the reviewed card"},
            "expected_content_version":2, "requested_at_ms":3
        }),
    )
    .await;
    assert_eq!(action["record"]["status"], "applied");
    let task = rt.tasks(&session).unwrap().pop().unwrap();
    assert_eq!(task.state, TaskState::Queued);
    let denied = RpcHandler::call(
        &rt,
        "artifacts.action_cancel",
        json!({
            "session_id":foreign, "action_id":"rpc-follow-up"
        }),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.code, "invalid_params");
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Queued);
    assert_eq!(
        rt.get_artifact_action("rpc-follow-up").unwrap().status,
        ActionStatus::Applied
    );
    let cancelled = rpc(
        &rt,
        "artifacts.action_cancel",
        json!({
            "session_id":session, "action_id":"rpc-follow-up"
        }),
    )
    .await;
    assert_eq!(cancelled["record"]["status"], "cancelled");
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Cancelled);
    let actions = rpc(&rt, "artifacts.action_list", json!({"session_id":session})).await;
    assert_eq!(actions.as_array().unwrap().len(), 1);
    let stored = rpc(
        &rt,
        "artifacts.action_get",
        json!({"action_id":"rpc-follow-up"}),
    )
    .await;
    assert_eq!(stored["status"], "cancelled");
    rpc(&rt, "artifacts.recovery_save", json!({})).await;
    rpc(
        &rt,
        "artifacts.update",
        json!({
            "artifact_id":"rpc-card", "session_id":session, "body":"<p>temporary</p>",
            "expected_content_version":2, "at_ms":4
        }),
    )
    .await;
    assert_eq!(
        rpc(&rt, "artifacts.recovery_load", json!({})).await["loaded"],
        true
    );
    assert_eq!(
        rpc(&rt, "artifacts.preview", json!({"artifact_id":"rpc-card"})).await["html"],
        "<p>reviewed</p>"
    );
}

#[tokio::test]
async fn removed_artifact_rpc_aliases_reject_without_side_effects() {
    let (_dir, rt) = open(vec![]);
    for method in [
        "artifact.put",
        "artifact.update",
        "artifact.preview",
        "artifact.list",
        "artifact.action",
        "artifact.action_list",
        "artifact.action_get",
        "artifact.action_cancel",
        "artifact.recovery_save",
        "artifact.recovery_load",
        "artifacts.action_dispatch",
        "artifacts.actions_list",
    ] {
        let error = RpcHandler::call(&rt, method, json!({}), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "method_not_found", "{method}");
    }
    assert!(rt.export_artifact_recovery().artifacts.artifacts.is_empty());
}

#[tokio::test]
async fn runtime_create_and_fork_reject_gateway_device_metadata() {
    let (_dir, rt) = open(vec![]);
    let error = RpcHandler::call(
        &rt,
        "session.create",
        json!({
            "label":"must-not-create", "device_id":"local"
        }),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(rt.list().unwrap().is_empty());
    let session = rt.create(None).unwrap().id;
    let error = RpcHandler::call(
        &rt,
        "session.fork",
        json!({
            "session_id":session, "label":"must-not-fork", "mode":"chat", "confirm":true,
            "device_id":"local"
        }),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert_eq!(rt.list().unwrap().len(), 1);
}

struct ControlledHost {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    started_tx: std::sync::mpsc::Sender<()>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    token: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    release: CancellationToken,
}

struct DispatchGuard {
    handle: Option<
        std::thread::JoinHandle<Result<cayw_runtime::ActionOutcome, cayw_runtime::RuntimeError>>,
    >,
    token: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    release: CancellationToken,
}

impl DispatchGuard {
    fn finish(mut self) -> cayw_runtime::ActionOutcome {
        self.release.cancel();
        self.handle
            .take()
            .unwrap()
            .join()
            .expect("dispatch thread should not panic")
            .expect("dispatch_artifact_action must return outcome")
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.lock().unwrap().as_ref() {
            token.cancel();
        }
        self.release.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct CancelGuard {
    token: CancellationToken,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.token.is_cancelled() {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl cayw_host::ExternalHost for ControlledHost {
    async fn call(
        &self,
        _capability: &str,
        _arguments: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, cayw_codemode::CapabilityError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _guard = CancelGuard {
            token: cancel.clone(),
            cancelled: self.cancelled.clone(),
        };
        *self.token.lock().unwrap() = Some(cancel.clone());
        let _ = self.started_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            cancel.cancelled().await;
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.release.cancelled().await;
            Err(cayw_codemode::CapabilityError::new("cancelled"))
        })
        .await
        .expect("test must cancel and release the controlled host")
    }
}

struct ControlledReconciliationVerifier;

impl ArtifactReconciliationVerifier for ControlledReconciliationVerifier {
    fn verify(&self, request: &ArtifactReconciliation) -> Result<(), String> {
        if request.actor != "trusted-test-host"
            || request.evidence
                != "controlled Host joined; token cancelled; original invocation counted once"
            || !matches!(&request.outcome, ArtifactReconciledOutcome::Failed { error } if error == "controlled Host cancelled after admission")
        {
            return Err("controlled Host evidence does not match".into());
        }
        Ok(())
    }
}

async fn reconcile_controlled_cancellation_and_continue(
    rt: &Runtime,
    session: &str,
    action_id: &str,
    calls: &std::sync::atomic::AtomicUsize,
) {
    assert!(!rt.is_tool_flight_active(action_id));
    assert_eq!(
        rt.get_artifact_action(action_id).unwrap().status,
        ActionStatus::OutcomeUnknown
    );
    assert!(matches!(
        rt.enqueue(session, "blocked until trusted cancellation reconciliation"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let audit = rt.audit(session, 0, 100).unwrap();
    assert!(
        audit
            .iter()
            .any(|event| event.kind == "artifact.action.outcome_unknown"
                && event.data["action_id"] == action_id)
    );
    let call_id = audit
        .iter()
        .find(|event| event.kind == "artifact.tool.started" && event.data["action_id"] == action_id)
        .unwrap()
        .data["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reconciled = rt
        .reconcile_artifact_action(ArtifactReconciliation {
            session_id: session.into(),
            action_id: action_id.into(),
            expected_artifact_id: "card-tool".into(),
            call_id: Some(call_id),
            actor: "trusted-test-host".into(),
            evidence: "controlled Host joined; token cancelled; original invocation counted once"
                .into(),
            outcome: ArtifactReconciledOutcome::Failed {
                error: "controlled Host cancelled after admission".into(),
            },
        })
        .unwrap();
    assert_eq!(reconciled.status, ActionStatus::Rejected);
    assert!(!rt.has_artifact_unknown(session).unwrap());
    let task = rt
        .enqueue(
            session,
            "continue original session after trusted reconciliation",
        )
        .unwrap();
    let completed = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        rt.resume(&task.id, CancellationToken::new()),
    )
    .await
    .expect("continued task must finish")
    .unwrap();
    assert_eq!(completed.id, session);
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Completed);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let audit = rt.audit(session, 0, 100).unwrap();
    assert_eq!(
        audit
            .iter()
            .filter(|event| event.kind == "artifact.tool.started"
                && event.data["action_id"] == action_id)
            .count(),
        1
    );
    assert_eq!(
        audit
            .iter()
            .filter(|event| event.kind == "artifact.action.reconciled"
                && event.data["action_id"] == action_id)
            .count(),
        1
    );
}

fn open_with_external(
    permissions: Vec<ScopedPermission>,
    external: Arc<dyn cayw_host::ExternalHost>,
) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(IdleProvider),
        Model::new("fixture", "artifact-dispatch"),
        RuntimeConfig {
            permissions,
            artifact_reconciliation_verifier: Some(Arc::new(ControlledReconciliationVerifier)),
            host: cayw_host::HostConfig {
                external: Some(external),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    (dir, rt)
}

#[tokio::test]
async fn controlled_host_tool_cancellation_and_unrelated_artifact_operations() {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let cancelled_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let token_holder = Arc::new(std::sync::Mutex::new(None));
    let release = CancellationToken::new();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = Arc::new(ControlledHost {
        calls: calls.clone(),
        started_tx,
        cancelled: cancelled_flag.clone(),
        token: token_holder.clone(),
        release: release.clone(),
    });
    let (_dir, rt) = open_with_external(
        vec![ScopedPermission::exact(
            "search.query",
            json!({"query": "wait-for-cancel"}),
        )],
        host,
    );
    let session = rt.create(Some("cancel-test".into())).unwrap().id;
    put_card(&rt, &session, "card-tool", "<form></form>");

    let rt_clone = rt.clone();
    let session_clone = session.clone();
    let dispatch_handle = std::thread::spawn(move || {
        rt_clone.dispatch_artifact_action(ActionRequest {
            action_id: "act-long".into(),
            artifact_id: "card-tool".into(),
            session_id: session_clone,
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "search.query".into(),
                args_json: "{\"query\":\"wait-for-cancel\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
    });
    let dispatch = DispatchGuard {
        handle: Some(dispatch_handle),
        token: token_holder.clone(),
        release,
    };

    // 1. Wait for deterministic handshake confirming Host operation started:
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("controlled host tool must start within timeout");

    // 2. While the tool is running, unrelated artifact operations must NOT be blocked:
    let put_res = rt.put_artifact(PutArtifactRequest {
        artifact_id: "card-unrelated".into(),
        session_id: session.clone(),
        kind: ArtifactKind::HtmlCard,
        title: "Unrelated".into(),
        body: "<p>unrelated</p>".into(),
        at_ms: 20,
        result_handle: None,
    });
    assert!(put_res.is_ok(), "put_artifact on unrelated artifact failed");
    let preview_res = rt
        .preview_artifact("card-unrelated")
        .expect("preview_artifact must succeed without blocking");
    assert_eq!(preview_res.html, "<p>unrelated</p>");
    let list_res = rt
        .list_artifacts(&session)
        .expect("list_artifacts must succeed without blocking");
    assert_eq!(list_res.len(), 2);

    // 3. Cancel the in-flight artifact action directly on the main thread:
    let cancel_outcome = rt
        .cancel_artifact_action(&session, "act-long")
        .expect("cancel_artifact_action must succeed without blocking on in-flight tool execution");
    assert_eq!(cancel_outcome.record.status, ActionStatus::Cancelled);

    // Cancellation after admission leaves the effect unknown until trusted reconciliation.
    let dispatch_result = dispatch.finish();
    assert_eq!(dispatch_result.record.status, ActionStatus::OutcomeUnknown);

    // 5. Verify the tool received cancellation on its CancellationToken:
    assert!(
        cancelled_flag.load(std::sync::atomic::Ordering::SeqCst),
        "CancellationToken must have been signalled before tool finished"
    );
    assert!(
        token_holder
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|t| t.is_cancelled()),
        "the tool CancellationToken must be in cancelled state"
    );

    // 6. Verify audit records:
    let audit = rt.audit(&session, 0, 100).unwrap();
    let kinds: Vec<_> = audit.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"artifact.action.cancelled"),
        "audit must contain artifact.action.cancelled, got {kinds:?}"
    );
    reconcile_controlled_cancellation_and_continue(&rt, &session, "act-long", &calls).await;
}

#[tokio::test]
async fn recovery_interleaved_with_inflight_dispatch_preserves_cancellation_and_semantics() {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let cancelled_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let token_holder = Arc::new(std::sync::Mutex::new(None));
    let release = CancellationToken::new();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = Arc::new(ControlledHost {
        calls: calls.clone(),
        started_tx,
        cancelled: cancelled_flag.clone(),
        token: token_holder.clone(),
        release: release.clone(),
    });
    let (_dir, rt) = open_with_external(
        vec![ScopedPermission::exact(
            "search.query",
            json!({"query": "wait-for-cancel"}),
        )],
        host,
    );
    let session = rt.create(Some("interleave-recovery".into())).unwrap().id;
    put_card(&rt, &session, "card-tool", "<form></form>");

    // Save baseline recovery on disk before dispatching action:
    let _ = rt.save_artifact_recovery().unwrap();

    let rt_clone = rt.clone();
    let session_clone = session.clone();
    let dispatch_handle = std::thread::spawn(move || {
        rt_clone.dispatch_artifact_action(ActionRequest {
            action_id: "act-recovering".into(),
            artifact_id: "card-tool".into(),
            session_id: session_clone,
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "search.query".into(),
                args_json: "{\"query\":\"wait-for-cancel\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
    });
    let dispatch = DispatchGuard {
        handle: Some(dispatch_handle),
        token: token_holder.clone(),
        release,
    };

    // 1. Wait for deterministic handshake confirming Host operation started:
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("controlled host tool must start within timeout");

    // A snapshot keeps the accepted identity for restart inspection.
    let live = rt.get_artifact_action("act-recovering").unwrap();
    assert_eq!(live.status, ActionStatus::Accepted);
    assert_eq!(
        rt.export_artifact_recovery().actions.actions,
        vec![live.clone()]
    );

    // Restore a baseline that predates this dispatch.
    rt.load_artifact_recovery()
        .expect("recovery reload must succeed");
    assert_eq!(rt.get_artifact_action("act-recovering").unwrap(), live);

    // A conflicting snapshot cannot replace the identity of the live dispatch.
    let mut conflicting = rt.export_artifact_recovery();
    let mut older_record = live.clone();
    older_record.status = ActionStatus::Applied;
    older_record.session_id = "older-session".into();
    conflicting.actions.actions = vec![older_record];
    rt.restore_artifact_recovery(conflicting.clone()).unwrap();
    assert_eq!(rt.get_artifact_action("act-recovering").unwrap(), live);

    // 3. Cancel the in-flight artifact action:
    let cancel_outcome = rt
        .cancel_artifact_action(&session, "act-recovering")
        .expect("cancel_artifact_action must reach in-flight flight even after recovery reload");
    assert_eq!(cancel_outcome.record.status, ActionStatus::Cancelled);

    // Cancellation after admission leaves the effect unknown until trusted reconciliation.
    let dispatch_result = dispatch.finish();
    assert_eq!(dispatch_result.record.status, ActionStatus::OutcomeUnknown);

    // 5. Verify the tool received cancellation on its CancellationToken:
    assert!(
        cancelled_flag.load(std::sync::atomic::Ordering::SeqCst),
        "CancellationToken must be cancelled"
    );

    // 6. Verify audit trail:
    let audit = rt.audit(&session, 0, 100).unwrap();
    let kinds: Vec<_> = audit.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"artifact.action.cancelled"),
        "audit must contain artifact.action.cancelled, got {kinds:?}"
    );
    reconcile_controlled_cancellation_and_continue(&rt, &session, "act-recovering", &calls).await;
}
