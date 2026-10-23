use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_cells::{CellIdentity, IntentStatus, SqliteJournal};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Script(AtomicUsize);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let sequence = self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(
                &request,
                "return cayw.call('fs.read',{path='a.txt'})",
                &format!("read-{sequence}"),
            ),
        }]))
    }
}
struct ReadReceipt(std::path::PathBuf);
impl cayw_core::ReconciliationVerifier for ReadReceipt {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        let cayw_core::ReconciliationRequest::Tool { reconciliation } = request else {
            return Err("shared-read fixture has no capability repair receipt".into());
        };
        let receipt: cayw_core::Reconciliation =
            serde_json::from_slice(&std::fs::read(&self.0).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if reconciliation == &receipt && receipt.actor == "fixture-host" {
            Ok(())
        } else {
            Err("shared-read reconciliation differs from its original fixture receipt".into())
        }
    }
}

fn config() -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config
        .permissions
        .push(ScopedPermission::exact("fs.read", json!({"path":"a.txt"})));
    config
}

// If acknowledgment is released despite a failed Cells write, the fault record
// disappears. If recovery rereads bytes, it records "changed after capture".
#[tokio::test]
async fn failed_cells_terminal_recovers_exact_dependencies_and_known_core_bytes_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "captured original bytes").unwrap();
    let path = dir.path().join("session.db");
    let provider = Arc::new(Script(AtomicUsize::new(0)));
    let open = || {
        let mut settings = config();
        settings.reconciliation_verifier =
            Some(Arc::new(ReadReceipt(dir.path().join("read-receipt.json"))));
        Runtime::open(
            &path,
            &root,
            provider.clone(),
            Model::new("fixture", "read-recovery"),
            settings,
        )
        .unwrap()
    };
    let rt = open();
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_original_read BEFORE UPDATE ON cells_v1 WHEN json_extract(NEW.body,'$.calls[0].status')='succeeded' AND json_array_length(json_extract(NEW.body,'$.host_settlements'))=0 BEGIN SELECT RAISE(ABORT,'fixture original shared-read Cells commit rejected'); END;").unwrap();
    let _result = rt.prompt(&id, "read once", CancellationToken::new()).await;
    let events = rt.events(&id, 0, 1000).unwrap();
    let call = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call.clone()),
            _ => None,
        })
        .unwrap();
    let Some(CapabilityOutcome::Completed { result }) = &call.outcome else {
        panic!("known Host/Core success was replaced: {call:?}");
    };
    assert_eq!(result["content"], "captured original bytes");
    let evidence = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data.clone()),
            _ => None,
        })
        .unwrap();
    let calls_before = provider.0.load(Ordering::SeqCst);
    let inspector =
        cayw_host::Host::new(&root, Arc::new(Deny), Arc::new(NoAudit), config().host).unwrap();
    let faults = inspector
        .pending_read_faults(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(faults.len(), 1);
    assert_eq!(
        faults[0].audit_id,
        format!("{}:{}", call.key.execution_id, call.key.sequence)
    );
    assert!(rt.enqueue(&id, "must stay frozen").is_err());
    // This deliberate out-of-protocol edit is a recovery probe. No new read or
    // capture may be used to reconstruct the original result or dependencies.
    std::fs::write(root.join("a.txt"), "changed after capture").unwrap();
    db.execute_batch("DROP TRIGGER reject_original_read")
        .unwrap();
    drop(rt);
    let rt = open();
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert!(recovered.unresolved_capabilities.is_empty());
    assert_eq!(recovered.unresolved_calls.len(), 1);
    let original_parent = recovered.unresolved_calls[0].call.id.clone();
    let expected = rt
        .store()
        .tool_reconciliation_origin(&id, &original_parent)
        .unwrap();
    assert_eq!(expected.lease, call.lease);
    assert_eq!(expected.task_id, call.task_id);
    assert_eq!(
        expected.execution.as_ref().unwrap().execution_id,
        call.key.execution_id
    );
    let receipt = cayw_core::Reconciliation {
        call_id: original_parent, expected, actor: "fixture-host".into(),
        evidence: "Original VM stopped; verified exact known Core read and repaired original Cells journal without replay".into(),
        outcome: cayw_core::ReconciledOutcome::Completed { result: cayw_agent::ToolResult::error("original read returned but its Cells terminal commit failed") },
    };
    std::fs::write(
        dir.path().join("read-receipt.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let mut forged = receipt.clone();
    forged.expected.lease.worker_id = "replacement-worker".into();
    assert!(rt.reconcile(&id, forged).is_err());
    rt.reconcile(&id, receipt).unwrap();
    let recovered = rt.get(&id).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(!recovered.frozen);
    assert!(
        inspector
            .pending_read_faults(CancellationToken::new())
            .await
            .unwrap()
            .is_empty(),
        "original acknowledged-read fence was never repaired"
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), calls_before);
    let journal = SqliteJournal::open(&path).unwrap();
    let identity = CellIdentity {
        session_id: id.clone(),
        task_id: call.task_id.clone(),
        attempt_id: call.lease.attempt_id.clone(),
        permission_context: format!("{}:{}", call.lease.worker_id, call.lease.fence),
    };
    let records = journal.records(&identity, &config().cells).unwrap();
    assert_eq!(records.len(), 1);
    let intent = &records[0].calls[0];
    assert_eq!(intent.status, IntentStatus::Succeeded);
    assert_eq!(
        intent.result.as_ref().unwrap()["content"],
        "captured original bytes"
    );
    assert_eq!(
        serde_json::to_value(intent.dependencies.as_ref().unwrap()).unwrap(),
        evidence["dependencies"]
    );
    assert!(records[0].backend_error.is_none());
    let calls_after: Vec<_> = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls_after, vec![call]);
    rt.recover(&id).unwrap();
    assert_eq!(provider.0.load(Ordering::SeqCst), calls_before);
}
// Reject either the canonical capture evidence or the original Core terminal.
// Recovery must keep the original unknown call and cannot reconstruct bytes from
// a deleted current file or dispatch another Provider turn.
#[tokio::test]
async fn failed_evidence_or_core_terminal_never_replays_or_guesses_the_capture() {
    for evidence_failure in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "captured once").unwrap();
        let path = dir.path().join("session.db");
        let provider = Arc::new(Script(AtomicUsize::new(0)));
        let rt = Runtime::open(
            &path,
            &root,
            provider.clone(),
            Model::new("fixture", "capture-persist-fault"),
            config(),
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let db = rusqlite::Connection::open(&path).unwrap();
        let condition = if evidence_failure {
            "json_extract(NEW.record,'$.event.kind')='runtime_shared_read'"
        } else {
            "json_extract(NEW.record,'$.event.type')='capability_settled'"
        };
        db.execute_batch(&format!("CREATE TRIGGER reject_original_capture BEFORE INSERT ON core_events WHEN {condition} BEGIN SELECT RAISE(ABORT,'fixture original capture persistence failed'); END;")).unwrap();
        assert!(
            rt.prompt(&id, "read once", CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        assert_eq!(rt.health().unwrap().running, 1);
        let before = rt.events(&id, 0, 1000).unwrap();
        assert_eq!(
            before
                .iter()
                .filter(
                    |e| matches!(&e.event,CoreEvent::Host {kind,..} if kind=="runtime_shared_read")
                )
                .count(),
            usize::from(!evidence_failure)
        );
        assert!(!before.iter().any(|e|matches!(&e.event,CoreEvent::CapabilitySettled {call} if matches!(call.outcome,Some(CapabilityOutcome::Completed {..})))));
        std::fs::remove_file(root.join("a.txt")).unwrap();
        db.execute_batch("DROP TRIGGER reject_original_capture")
            .unwrap();
        drop(rt);
        let rt = Runtime::open(
            &path,
            &root,
            provider.clone(),
            Model::new("fixture", "capture-persist-fault"),
            config(),
        )
        .unwrap();
        let recovered = rt.recover(&id).unwrap();
        assert_eq!(recovered.state, SessionState::NeedsReconciliation);
        assert_eq!(recovered.unresolved_capabilities.len(), 1);
        assert!(matches!(
            recovered.unresolved_capabilities[0].outcome,
            Some(CapabilityOutcome::OutcomeUnknown { .. })
        ));
        assert!(rt.enqueue(&id, "must inspect original read").is_err());
        let inspector =
            cayw_host::Host::new(&root, Arc::new(Deny), Arc::new(NoAudit), config().host).unwrap();
        assert_eq!(
            inspector
                .pending_read_faults(CancellationToken::new())
                .await
                .unwrap()
                .len(),
            1
        );
        let journal = SqliteJournal::open(&path).unwrap();
        let call = &recovered.unresolved_capabilities[0];
        let identity = CellIdentity {
            session_id: id.clone(),
            task_id: call.task_id.clone(),
            attempt_id: call.lease.attempt_id.clone(),
            permission_context: format!("{}:{}", call.lease.worker_id, call.lease.fence),
        };
        let records = journal.records(&identity, &config().cells).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].calls[0].result.is_none());
        assert_ne!(records[0].calls[0].status, IntentStatus::Succeeded);
        rt.recover(&id).unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        assert!(!root.join("a.txt").exists());
    }
}
struct Deny;
#[async_trait::async_trait]
impl cayw_host::PermissionGate for Deny {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), cayw_codemode::CapabilityError> {
        Err(cayw_codemode::CapabilityError::new(
            "inspection cannot execute",
        ))
    }
}
struct NoAudit;
#[async_trait::async_trait]
impl cayw_host::AuditSink for NoAudit {
    async fn started(&self, _: &str, _: &Value) -> Result<String, cayw_codemode::CapabilityError> {
        panic!("inspection cannot start capabilities")
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, cayw_codemode::CapabilityError>,
    ) -> Result<(), cayw_codemode::CapabilityError> {
        panic!("inspection cannot settle capabilities")
    }
    fn dropped(&self, _: &str) -> Result<(), cayw_codemode::CapabilityError> {
        panic!("inspection has no capability")
    }
}
