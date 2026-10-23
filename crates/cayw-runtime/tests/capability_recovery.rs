use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityOutcome, CapabilityReconciliation, CoreEvent, SessionState};
use cayw_host::ExternalHost;
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use futures::stream;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ReceiptVerifier {
    directory: std::path::PathBuf,
}
impl cayw_core::ReconciliationVerifier for ReceiptVerifier {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        let input = match request {
            cayw_core::ReconciliationRequest::Tool { reconciliation } => {
                serde_json::to_value(reconciliation)
            }
            cayw_core::ReconciliationRequest::Capability { reconciliation } => {
                serde_json::to_value(reconciliation)
            }
        }
        .map_err(|error| error.to_string())?;
        if input["actor"] != "fixture-host" {
            return Err("this host has not authorized that actor".into());
        }
        let path = std::path::Path::new(input["evidence"].as_str().ok_or("missing receipt")?);
        if path.parent() != Some(self.directory.as_path()) {
            return Err("receipt is outside the trusted host's directory".into());
        }
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if receipt == input {
            Ok(())
        } else {
            Err("receipt does not verify this exact origin and decision".into())
        }
    }
}
fn host_receipt<T: serde::Serialize + serde::de::DeserializeOwned>(
    dir: &tempfile::TempDir,
    input: T,
) -> T {
    let directory = dir.path().join("trusted-host-receipts");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{}.json", uuid::Uuid::new_v4()));
    let mut value = serde_json::to_value(input).unwrap();
    value["evidence"] = json!(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    file.sync_all().unwrap();
    std::fs::File::open(directory).unwrap().sync_all().unwrap();
    serde_json::from_value(value).unwrap()
}

struct Script {
    script: &'static str,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = if request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { content, .. } => Some(cayw_ai::content_text(content)),
                _ => None,
            })
            .as_deref()
            == Some("explicit next task")
        {
            let mut answer = cayw_ai::AssistantMessage::new(&request.model);
            answer.content = vec![cayw_ai::Content::text("verified next task without replay")];
            answer.stop_reason = cayw_ai::StopReason::Stop;
            answer
        } else {
            cayw_runtime::fixture::script_response(&request, self.script, &format!("call-{n}"))
        };
        Box::pin(stream::iter(vec![AssistantEvent::Done { message: answer }]))
    }
}
struct UnknownService(AtomicUsize);
#[async_trait]
impl ExternalHost for UnknownService {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(CapabilityError::unknown(
            "synthetic external response lost after side effect boundary",
        ))
    }
}
enum FileServiceOutcome {
    KnownFailure,
    UnknownEffect,
}
struct FileService {
    directory: std::path::PathBuf,
    outcome: FileServiceOutcome,
    calls: AtomicUsize,
}
fn durable_service_file(path: &std::path::Path, bytes: &[u8]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    std::fs::File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
}
#[async_trait]
impl ExternalHost for FileService {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        assert_eq!(name, "search.query");
        assert_eq!(arguments, json!({"query":"test"}));
        assert_eq!(
            self.calls.fetch_add(1, Ordering::SeqCst),
            0,
            "the original external service must not be replayed"
        );
        let receipt = match self.outcome {
            FileServiceOutcome::KnownFailure => {
                let error = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(self.directory.join("blocked-effect"))
                    .expect_err("create_new must encounter the actual directory collision");
                json!({
                    "kind": "known_create_new_failure", "name": name, "arguments": arguments,
                    "raw_os_error": error.raw_os_error(),
                    "error": format!("known external create_new failure: {error}"),
                })
            }
            FileServiceOutcome::UnknownEffect => {
                durable_service_file(
                    &self.directory.join("effect.txt"),
                    b"one actual external effect",
                );
                json!({
                    "kind": "durable_external_effect", "name": name, "arguments": arguments,
                    "result": {"count": 1, "effect": "one actual external effect"},
                })
            }
        };
        durable_service_file(
            &self.directory.join("service-receipt.json"),
            &serde_json::to_vec(&receipt).unwrap(),
        );
        match self.outcome {
            FileServiceOutcome::KnownFailure => {
                Err(CapabilityError::new(receipt["error"].as_str().unwrap()))
            }
            FileServiceOutcome::UnknownEffect => Err(CapabilityError::unknown(
                "the actual external effect is durable, but its response was lost",
            )),
        }
    }
}
struct AllowTestReconciliation(AtomicUsize);
impl cayw_core::ReconciliationVerifier for AllowTestReconciliation {
    fn verify(&self, _: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        // Intentionally broad: this regression must exercise independent local
        // contradiction checks after host authorization has already succeeded.
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn stored_cell(db: &rusqlite::Connection, session: &str) -> (i64, String) {
    db.query_row(
        "SELECT revision,body FROM cells_v1 WHERE session=?1",
        [session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}
fn open(dir: &tempfile::TempDir, provider: Arc<Script>, mut config: RuntimeConfig) -> Runtime {
    config.reconciliation_verifier = Some(Arc::new(ReceiptVerifier {
        directory: dir.path().join("trusted-host-receipts"),
    }));
    Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("workspace"),
        provider,
        Model::new("fixture", "audit"),
        config,
    )
    .unwrap()
}
fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    dir
}

#[tokio::test]
async fn real_host_unknown_terminal_pairs_in_core_and_blocks_models_after_reopen() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "cayw.call('search.query',{query='test'}); return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&id, "unknown external effect", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::NeedsReconciliation);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let events = rt.events(&id, 0, 1000).unwrap();
    let start = events
        .iter()
        .find_map(|record| match &record.event {
            CoreEvent::CapabilityStarted { call } => Some(call),
            _ => None,
        })
        .unwrap();
    let end = events
        .iter()
        .find_map(|record| match &record.event {
            CoreEvent::CapabilitySettled { call }
                if matches!(
                    call.outcome,
                    Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
                ) =>
            {
                Some(call)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(start.key, end.key);
    assert_eq!(start.parent_call_id, end.parent_call_id);
    assert_eq!(start.task_id, end.task_id);
    assert_eq!(start.lease, end.lease);
    assert_eq!(start.parent_call_id, "call-0");
    assert_eq!(start.key.sequence, 0);
    assert_eq!(result.unresolved_capabilities.len(), 1);
    assert_eq!(result.unresolved_capabilities[0], *end);
    assert!(result.unresolved_calls.is_empty());
    assert!(result.messages.iter().any(|message| {
        matches!(message, Message::ToolResult(result)
            if result.is_error && !result.outcome_unknown
                && result.structured_content.as_ref().is_some_and(|page| page["outcome_unknown"] == true))
    }));
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.prompt(&id, "try again", CancellationToken::new()).await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    rt.recover(&id).unwrap();
    assert!(matches!(
        rt.prompt(&id, "still blocked", CancellationToken::new())
            .await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let reconciled = rt.reconcile_capability(&id, host_receipt(&dir, CapabilityReconciliation {
        expected: end.into(),
        key: end.key.clone(),
        actor: "fixture-host".into(),
        evidence: "Synthetic service counter observed one completed test invocation; no real provider used".into(),
        outcome: CapabilityOutcome::Completed { result: json!({"count":1}) },
        external_provenance: end.external_provenance.clone(),
    })).unwrap();
    let view = serde_json::to_value(reconciled).unwrap();
    assert_eq!(view["frozen"], false);
    assert_eq!(view["binding"], serde_json::Value::Null);
    assert_eq!(rt.get(&id).unwrap().state, SessionState::Idle);
    rt.enqueue(&id, "explicit task after verified capability")
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cells_commit_failure_after_core_reconciliation_is_frozen_and_retryable() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "lose one response", CancellationToken::new())
        .await
        .unwrap();
    let call = view.unresolved_capabilities[0].clone();
    rt.recover(&id).unwrap();
    let input = host_receipt(
        &dir,
        CapabilityReconciliation {
            expected: (&call).into(),
            key: call.key.clone(),
            actor: "fixture-host".into(),
            evidence: "Observed the synthetic service counter at exactly one invocation".into(),
            outcome: CapabilityOutcome::Completed {
                result: json!({"count":1}),
            },
            external_provenance: call.external_provenance.clone(),
        },
    );
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_cells_repair BEFORE UPDATE ON cells_v1
        WHEN json_array_length(json_extract(NEW.body,'$.host_settlements')) >
             json_array_length(json_extract(OLD.body,'$.host_settlements'))
        BEGIN SELECT RAISE(ABORT,'synthetic Cells repair failure'); END;",
    )
    .unwrap();
    let error = rt.reconcile_capability(&id, input.clone()).unwrap_err();
    assert!(
        error.to_string().contains("synthetic Cells repair failure"),
        "{error}"
    );
    assert_eq!(rt.store().snapshot(&id).unwrap().state, SessionState::Idle);
    assert!(matches!(
        rt.enqueue(&id, "blocked during partial commit"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
    db.execute_batch("DROP TRIGGER reject_cells_repair")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.enqueue(&id, "still blocked after reopen"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    let recovered = rt.reconcile_capability(&id, input.clone()).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    let events = rt.events(&id, 0, 1000).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, CoreEvent::CapabilityReconciled { .. }))
            .count(),
        1
    );
    let body: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1",
            [&call.key.execution_id],
            |row| row.get(0),
        )
        .unwrap();
    let first: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(first["calls"][0]["status"], "succeeded");
    assert_eq!(first["calls"][0]["result"], json!({"count":1}));
    assert!(first["backend_error"].is_null());
    assert!(!first["host_settlements"].as_array().unwrap().is_empty());
    rt.reconcile_capability(&id, input.clone()).unwrap();
    let retried: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1",
            [&call.key.execution_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        body, retried,
        "an exact retry must not change Cells revision or its audit"
    );
    let mut changed = input;
    changed.outcome = CapabilityOutcome::Completed {
        result: json!({"count":2}),
    };
    let changed = host_receipt(&dir, changed);
    assert!(
        rt.reconcile_capability(&id, changed).is_err(),
        "a second trusted receipt cannot rewrite the original committed decision"
    );
    assert_eq!(rt.get(&id).unwrap(), recovered);
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn known_failed_external_child_rejects_parent_not_executed_before_core_commit() {
    let dir = setup();
    std::fs::create_dir(dir.path().join("blocked-effect")).unwrap();
    let service = Arc::new(FileService {
        directory: dir.path().into(),
        outcome: FileServiceOutcome::KnownFailure,
        calls: AtomicUsize::new(0),
    });
    let verifier = Arc::new(AllowTestReconciliation(AtomicUsize::new(0)));
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    config.reconciliation_verifier = Some(verifier.clone());
    let rt = Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "audit"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_known_failure_execution_finish BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='execution_finished'
        BEGIN SELECT RAISE(ABORT,'known failure backend lifecycle write failure'); END;",
    )
    .unwrap();
    let error = rt
        .prompt(
            &id,
            "execute the directory collision",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("known failure backend lifecycle write failure"),
        "{error}"
    );
    assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let receipt_bytes = std::fs::read(dir.path().join("service-receipt.json")).unwrap();
    let receipt: Value = serde_json::from_slice(&receipt_bytes).unwrap();
    assert_eq!(receipt["kind"], "known_create_new_failure");
    assert!(receipt["raw_os_error"].as_i64().is_some());
    assert!(dir.path().join("blocked-effect").is_dir());
    let failed_before_recovery: Value = serde_json::from_str(&stored_cell(&db, &id).1).unwrap();
    assert_eq!(failed_before_recovery["calls"][0]["status"], "failed");
    assert_eq!(
        failed_before_recovery["calls"][0]["error"],
        receipt["error"]
    );
    assert!(failed_before_recovery["calls"][0]["result"].is_null());
    assert!(failed_before_recovery["calls"][0]["result_hash"].is_null());
    assert!(
        failed_before_recovery["backend_error"]
            .as_str()
            .unwrap()
            .contains("known failure backend lifecycle write failure")
    );
    db.execute_batch("DROP TRIGGER reject_known_failure_execution_finish;")
        .unwrap();
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert!(recovered.unresolved_capabilities.is_empty());
    let cells_before = stored_cell(&db, &id);
    let recovered_cell: Value = serde_json::from_str(&cells_before.1).unwrap();
    assert_eq!(
        recovered_cell["calls"][0], failed_before_recovery["calls"][0],
        "recovery must preserve the known failed executor entry"
    );
    assert!(recovered_cell["backend_error"].is_null());
    let call_id = recovered.unresolved_calls[0].call.id.clone();
    let input = cayw_core::Reconciliation {
        expected: rt
            .store()
            .tool_reconciliation_origin(&id, &call_id)
            .unwrap(),
        call_id,
        actor: "authorized-fixture-host".into(),
        evidence: dir
            .path()
            .join("service-receipt.json")
            .to_string_lossy()
            .into(),
        outcome: cayw_core::ReconciledOutcome::NotExecuted {
            reason: "deliberately stale claim that the original parent never executed".into(),
        },
    };
    assert!(input.expected.execution.is_some());
    let core_before = rt.store().snapshot(&id).unwrap();
    let events_before = rt.events(&id, 0, 1000).unwrap();
    let messages_before = recovered.messages.clone();
    assert!(
        messages_before.iter().any(
            |message| matches!(message, Message::ToolResult(result) if result.outcome_unknown)
        )
    );
    let frozen_before: i64 = db
        .query_row(
            "SELECT count(*) FROM cells_frozen_v1 WHERE session=?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    verifier.0.store(0, Ordering::SeqCst);
    let decision = rt.reconcile(&id, input);
    let core_after = rt.store().snapshot(&id).unwrap();
    let cells_after = stored_cell(&db, &id);
    let messages_after = rt.get(&id).unwrap().messages;
    assert!(
        verifier.0.load(Ordering::SeqCst) > 0,
        "the broad test verifier must authorize before local contradiction checks"
    );
    assert!(
        decision.is_err(),
        "a known failed child proves the parent executed; reject before Core commit: \
         decision={decision:?}, core_unchanged={}, cells_unchanged={}, messages_unchanged={}",
        core_after == core_before,
        cells_after == cells_before,
        messages_after == messages_before,
    );
    assert_eq!(core_after, core_before);
    assert_eq!(rt.events(&id, 0, 1000).unwrap(), events_before);
    assert_eq!(cells_after, cells_before);
    let frozen_after: i64 = db
        .query_row(
            "SELECT count(*) FROM cells_frozen_v1 WHERE session=?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(frozen_after, frozen_before);
    assert_eq!(messages_after, messages_before);
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(dir.path().join("service-receipt.json")).unwrap(),
        receipt_bytes
    );
    assert!(dir.path().join("blocked-effect").is_dir());
}

#[tokio::test]
async fn partial_cells_reconciliation_blocks_fork_until_exact_retry_then_continues_once() {
    let dir = setup();
    let service = Arc::new(FileService {
        directory: dir.path().into(),
        outcome: FileServiceOutcome::UnknownEffect,
        calls: AtomicUsize::new(0),
    });
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let unknown = rt
        .prompt(&id, "persist one external effect", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(unknown.state, SessionState::NeedsReconciliation);
    assert_eq!(unknown.unresolved_capabilities.len(), 1);
    assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let effect = std::fs::read(dir.path().join("effect.txt")).unwrap();
    assert_eq!(effect, b"one actual external effect");
    let receipt_bytes = std::fs::read(dir.path().join("service-receipt.json")).unwrap();
    let receipt: Value = serde_json::from_slice(&receipt_bytes).unwrap();
    assert_eq!(receipt["kind"], "durable_external_effect");
    let call = unknown.unresolved_capabilities[0].clone();
    rt.recover(&id).unwrap();
    let input = host_receipt(
        &dir,
        CapabilityReconciliation {
            expected: (&call).into(),
            key: call.key.clone(),
            actor: "fixture-host".into(),
            evidence: String::new(),
            outcome: CapabilityOutcome::Completed {
                result: receipt["result"].clone(),
            },
            external_provenance: call.external_provenance.clone(),
        },
    );
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    let cells_before = stored_cell(&db, &id);
    db.execute_batch(
        "CREATE TRIGGER reject_fork_cells_repair BEFORE UPDATE ON cells_v1
        WHEN json_array_length(json_extract(NEW.body,'$.host_settlements')) >
             json_array_length(json_extract(OLD.body,'$.host_settlements'))
        BEGIN SELECT RAISE(ABORT,'partial Fork Cells repair failure'); END;",
    )
    .unwrap();
    let error = rt.reconcile_capability(&id, input.clone()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("partial Fork Cells repair failure"),
        "{error}"
    );
    let core_partial = rt.store().snapshot(&id).unwrap();
    assert_eq!(core_partial.state, SessionState::Idle);
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(stored_cell(&db, &id), cells_before);
    assert!(matches!(
        rt.enqueue(&id, "explicit next task"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    let sessions_before = rt.store().list_sessions().unwrap();
    let events_before = rt.events(&id, 0, 1000).unwrap();
    let tasks_before = rt.tasks(&id).unwrap();
    let fork = rt.fork(&id, "must wait for original Cells repair", true);
    let sessions_after = rt.store().list_sessions().unwrap();
    assert!(
        matches!(fork, Err(RuntimeError::NeedsReconciliation)),
        "confirmed Fork must reject incomplete Cells repair: result={fork:?}, \
         sessions_before={}, sessions_after={}",
        sessions_before.len(),
        sessions_after.len(),
    );
    assert_eq!(
        sessions_after, sessions_before,
        "a rejected Fork must not create a new Core Session"
    );
    assert_eq!(rt.store().snapshot(&id).unwrap(), core_partial);
    assert_eq!(rt.events(&id, 0, 1000).unwrap(), events_before);
    assert_eq!(rt.tasks(&id).unwrap(), tasks_before);
    assert_eq!(stored_cell(&db, &id), cells_before);
    assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    db.execute_batch("DROP TRIGGER reject_fork_cells_repair;")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.enqueue(&id, "explicit next task"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    let repaired = rt.reconcile_capability(&id, input.clone()).unwrap();
    assert_eq!(repaired.state, SessionState::Idle);
    assert_eq!(repaired.messages, unknown.messages);
    let repaired_cell = stored_cell(&db, &id);
    let body: Value = serde_json::from_str(&repaired_cell.1).unwrap();
    assert_eq!(body["calls"][0]["status"], "succeeded");
    assert_eq!(body["calls"][0]["result"], receipt["result"]);
    let before_body: Value = serde_json::from_str(&cells_before.1).unwrap();
    let previous_settlements = before_body["host_settlements"].as_array().unwrap();
    let repaired_settlements = body["host_settlements"].as_array().unwrap();
    assert!(repaired_settlements.starts_with(previous_settlements));
    assert_eq!(repaired_settlements.len(), previous_settlements.len() + 1);
    rt.reconcile_capability(&id, input).unwrap();
    assert_eq!(stored_cell(&db, &id), repaired_cell);
    let source_before = rt.store().snapshot(&id).unwrap();
    let source_events = rt.events(&id, 0, 1000).unwrap();
    assert_eq!(
        source_events
            .iter()
            .filter(|event| matches!(event.event, CoreEvent::CapabilityReconciled { .. }))
            .count(),
        1
    );
    let fork = rt
        .fork(&id, "confirmed after original Cells repair", true)
        .unwrap();
    assert_eq!(fork.messages, source_before.messages);
    assert_eq!(
        rt.store().list_sessions().unwrap().len(),
        sessions_before.len() + 1
    );
    let fork_events = rt.events(&fork.id, 0, 1000).unwrap();
    let completed = rt
        .prompt(&fork.id, "explicit next task", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.state, SessionState::Idle);
    assert!(completed.messages.starts_with(&fork.messages));
    assert!(matches!(
        completed.messages.last(),
        Some(Message::Assistant(answer))
            if answer.stop_reason == cayw_ai::StopReason::Stop
                && cayw_ai::content_text(&answer.content) == "verified next task without replay"
    ));
    assert_eq!(rt.tasks(&fork.id).unwrap().len(), 1);
    assert_eq!(
        rt.tasks(&fork.id).unwrap()[0].state,
        cayw_runtime::TaskState::Completed
    );
    assert_eq!(completed.executions, fork.executions);
    let completed_events = rt.events(&fork.id, 0, 1000).unwrap();
    assert!(completed_events.starts_with(&fork_events));
    assert!(
        completed_events
            .iter()
            .skip(fork_events.len())
            .any(|record| {
                matches!(&record.event, CoreEvent::Agent { event }
            if matches!(event.as_ref(), cayw_agent::AgentEvent::AgentEnd { .. }))
            })
    );
    assert_eq!(rt.store().snapshot(&id).unwrap(), source_before);
    assert_eq!(rt.events(&id, 0, 1000).unwrap(), source_events);
    assert_eq!(stored_cell(&db, &id), repaired_cell);
    assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read(dir.path().join("effect.txt")).unwrap(),
        effect
    );
    assert_eq!(
        std::fs::read(dir.path().join("service-receipt.json")).unwrap(),
        receipt_bytes
    );
}

#[tokio::test]
async fn stopped_cells_owner_rejects_reconciliation_before_core_is_changed() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "lose response", CancellationToken::new())
        .await
        .unwrap();
    let call = view.unresolved_capabilities[0].clone();
    let journal =
        Arc::new(cayw_cells::SqliteJournal::open(dir.path().join("sessions.db")).unwrap());
    let identity = cayw_cells::CellIdentity {
        session_id: id.clone(),
        task_id: call.task_id.clone(),
        attempt_id: call.lease.attempt_id.clone(),
        permission_context: format!("{}:{}", call.lease.worker_id, call.lease.fence),
    };
    let owned = journal
        .stopped_scope(
            cayw_cells::ExecutorStopped {
                identity,
                evidence: "Independent host owns the stopped scope for this test".into(),
            },
            config.cells,
        )
        .unwrap();
    let before = rt.get(&id).unwrap();
    let input = host_receipt(
        &dir,
        CapabilityReconciliation {
            expected: (&call).into(),
            key: call.key.clone(),
            actor: "fixture-host".into(),
            evidence: "Synthetic service observed exactly once".into(),
            outcome: CapabilityOutcome::Completed {
                result: json!({"count":1}),
            },
            external_provenance: call.external_provenance,
        },
    );
    assert!(rt.reconcile_capability(&id, input.clone()).is_err());
    assert_eq!(rt.get(&id).unwrap(), before);
    assert!(rt.recover(&id).is_err());
    assert_eq!(rt.get(&id).unwrap(), before);
    drop(owned);
    assert_eq!(
        rt.reconcile_capability(&id, input).unwrap().state,
        SessionState::Idle
    );
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn backend_terminal_failure_repairs_from_core_without_replaying_the_file_write() {
    let dir = setup();
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"once.txt","content":"one effect"}),
        )],
        ..Default::default()
    };
    let provider = Arc::new(Script {
        script: "return cayw.call('fs.write',{path='once.txt',content='one effect'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_execution_finish BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='execution_finished'
        BEGIN SELECT RAISE(ABORT,'synthetic backend lifecycle failure'); END;",
    )
    .unwrap();
    assert!(
        rt.prompt(&id, "write once", CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/once.txt")).unwrap(),
        "one effect"
    );
    assert_eq!(rt.health().unwrap().running, 1);
    let body: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    let before: Value = serde_json::from_str(&body).unwrap();
    assert!(
        before["backend_error"]
            .as_str()
            .unwrap()
            .contains("synthetic backend lifecycle failure")
    );
    let call = before["calls"][0].clone();
    db.execute_batch(
        "DROP TRIGGER reject_execution_finish;
        CREATE TRIGGER reject_recovery_cells BEFORE UPDATE ON cells_v1
        WHEN json_array_length(json_extract(NEW.body,'$.host_settlements')) >
             json_array_length(json_extract(OLD.body,'$.host_settlements'))
        BEGIN SELECT RAISE(ABORT,'synthetic stopped Cells write failure'); END;",
    )
    .unwrap();
    assert!(
        rt.recover(&id)
            .unwrap_err()
            .to_string()
            .contains("synthetic stopped Cells write failure")
    );
    assert!(rt.get(&id).unwrap().attempt.is_none());
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "failed Cells sync must retain its slot"
    );
    assert!(rt.enqueue(&id, "must remain frozen").is_err());
    db.execute_batch("DROP TRIGGER reject_recovery_cells")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert!(recovered.unresolved_capabilities.is_empty());
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(
        rt.enqueue(&id, "parent reply still needs host evidence")
            .is_err()
    );
    let repaired: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    let after: Value = serde_json::from_str(&repaired).unwrap();
    assert!(after["backend_error"].is_null());
    assert_eq!(
        after["calls"][0], call,
        "known original result must remain intact"
    );
    assert_eq!(after["host_settlements"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["host_settlements"][0]["settlement"]["execution_terminal"]["expected_backend_error"],
        before["backend_error"]
    );
    rt.recover(&id).unwrap();
    let retried: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        repaired, retried,
        "recovery retry must not add duplicate audit records"
    );
    let original_messages = rt.get(&id).unwrap().messages;
    let original_events = rt.events(&id, 0, 1000).unwrap();
    let contradictory = host_receipt(
        &dir,
        cayw_core::Reconciliation {
            expected: rt
                .store()
                .tool_reconciliation_origin(&id, &recovered.unresolved_calls[0].call.id)
                .unwrap(),
            call_id: recovered.unresolved_calls[0].call.id.clone(),
            actor: "fixture-host".into(),
            evidence: String::new(),
            outcome: cayw_core::ReconciledOutcome::NotExecuted {
                reason: "claims no execution despite the reliable original child write".into(),
            },
        },
    );
    let original_core = rt.store().snapshot(&id).unwrap();
    assert!(
        rt.reconcile(&id, contradictory).is_err(),
        "the known child result disproves a parent nonexecution claim"
    );
    assert_eq!(rt.store().snapshot(&id).unwrap(), original_core);
    assert_eq!(rt.events(&id, 0, 1000).unwrap(), original_events);
    let input = host_receipt(&dir, cayw_core::Reconciliation {
        expected: rt.store().tool_reconciliation_origin(&id, &recovered.unresolved_calls[0].call.id).unwrap(),
        call_id: recovered.unresolved_calls[0].call.id.clone(), actor: "fixture-host".into(),
        evidence: "Observed written once.txt and matched the durable child result; only the launch reply was lost".into(),
        outcome: cayw_core::ReconciledOutcome::Completed {
            result: cayw_agent::ToolResult::error("VM stopped after the known child result; inspect its original execution"),
        },
    });
    assert_eq!(
        rt.reconcile(&id, input.clone()).unwrap().state,
        SessionState::Idle
    );
    let reconciled = rt.get(&id).unwrap();
    rt.reconcile(&id, input).unwrap();
    assert_eq!(rt.get(&id).unwrap(), reconciled);
    let final_record: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(repaired, final_record);
    assert_eq!(rt.get(&id).unwrap().messages, original_messages);
    assert_eq!(
        rt.events(&id, 0, original_events.len()).unwrap(),
        original_events
    );
    assert!(
        original_messages.iter().any(
            |message| matches!(message, Message::ToolResult(result) if result.outcome_unknown)
        )
    );
    let completed = rt
        .prompt(&id, "explicit next task", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.state, SessionState::Idle);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        &completed.messages[..original_messages.len()],
        original_messages
    );
    assert_eq!(
        completed.executions.len(),
        1,
        "the original VM was not replayed"
    );
    assert_eq!(
        rt.events(&id, 0, original_events.len()).unwrap(),
        original_events
    );
    assert!(rt.events(&id, reconciled.cursor, 1000).unwrap().iter().any(|event| matches!(&event.event,
        CoreEvent::Agent { event } if matches!(event.as_ref(), cayw_agent::AgentEvent::AgentEnd { .. }))));
    let reopened = open(&dir, provider.clone(), RuntimeConfig::default());
    assert_eq!(
        &reopened.get(&id).unwrap().messages[..original_messages.len()],
        original_messages
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/once.txt")).unwrap(),
        "one effect"
    );
}

#[tokio::test]
async fn recovering_several_completed_tasks_uses_one_session_gate() {
    let dir = setup();
    let provider = Arc::new(Script {
        script: "return 'known'",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), RuntimeConfig::default());
    let id = rt.create(None).unwrap().id;
    for index in 0..3 {
        assert_eq!(
            rt.prompt(&id, &format!("task {index}"), CancellationToken::new())
                .await
                .unwrap()
                .state,
            SessionState::Idle
        );
    }
    assert_eq!(rt.tasks(&id).unwrap().len(), 3);
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.health().unwrap().queued, 1);
}

#[tokio::test]
async fn settlement_persistence_failure_retains_written_file_and_unknown_parent() {
    let dir = setup();
    let provider = Arc::new(Script {
        script: "return cayw.call('fs.write',{path='committed.txt',content='written'})",
        calls: AtomicUsize::new(0),
    });
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"committed.txt","content":"written"}),
        )],
        ..Default::default()
    };
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER runtime_test_reject_settlement BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='capability_settled'
        BEGIN SELECT RAISE(ABORT,'synthetic settlement storage failure'); END;",
    )
    .unwrap();
    let result = rt
        .prompt(
            &id,
            "write with failed settlement journal",
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/committed.txt")).unwrap(),
        "written"
    );
    let snapshot = rt.get(&id).unwrap();
    assert_eq!(
        snapshot.state,
        SessionState::NeedsReconciliation,
        "{result:?}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        snapshot
            .unresolved_calls
            .iter()
            .any(|call| call.call.id == "call-0")
    );
    db.execute_batch("DROP TRIGGER runtime_test_reject_settlement")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.prompt(&id, "do not replay", CancellationToken::new())
            .await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reconciliation_requires_host_authority_and_an_exact_durable_receipt() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let unknown = rt
        .prompt(&id, "unknown receipt", CancellationToken::new())
        .await
        .unwrap();
    let call = unknown.unresolved_capabilities[0].clone();
    let original = rt.store().snapshot(&id).unwrap();
    let events = rt.events(&id, 0, 1000).unwrap();
    let input = host_receipt(
        &dir,
        CapabilityReconciliation {
            expected: (&call).into(),
            key: call.key.clone(),
            actor: "fixture-host".into(),
            evidence: String::new(),
            outcome: CapabilityOutcome::Completed {
                result: json!({"count":1}),
            },
            external_provenance: call.external_provenance.clone(),
        },
    );
    // Same store and exact input, but a peer host has no reconciliation policy.
    let untrusted = Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "audit"),
        config,
    )
    .unwrap();
    assert!(matches!(
        untrusted.reconcile_capability(&id, input.clone()),
        Err(RuntimeError::Core(cayw_core::CoreError::ApprovalDenied))
    ));
    for field in ["actor", "missing", "result", "origin"] {
        let mut changed = input.clone();
        match field {
            "actor" => changed.actor = "rpc-user".into(),
            "missing" => {
                changed.evidence = dir
                    .path()
                    .join("trusted-host-receipts/missing.json")
                    .to_string_lossy()
                    .into()
            }
            "result" => {
                changed.outcome = CapabilityOutcome::Completed {
                    result: json!({"count":2}),
                }
            }
            _ => changed.expected.lease.worker_id = "other-worker".into(),
        }
        assert!(rt.reconcile_capability(&id, changed).is_err(), "{field}");
        assert_eq!(rt.store().snapshot(&id).unwrap(), original);
        assert_eq!(rt.events(&id, 0, 1000).unwrap(), events);
        assert!(
            !rt.get(&id).unwrap().frozen,
            "rejected evidence must not acquire or freeze stopped ownership"
        );
    }
    for field in ["task", "lease", "source"] {
        let mut changed = input.clone();
        match field {
            "task" => changed.expected.task_id = "other-task".into(),
            "lease" => changed.expected.lease.fence += 1,
            _ => changed.expected.source_hash = "different-source".into(),
        }
        let changed = host_receipt(&dir, changed);
        assert!(
            rt.reconcile_capability(&id, changed).is_err(),
            "host authorization cannot replace origin checks: {field}"
        );
        assert_eq!(rt.store().snapshot(&id).unwrap(), original);
        assert_eq!(rt.events(&id, 0, 1000).unwrap(), events);
    }
    assert_eq!(
        rt.reconcile_capability(&id, input.clone()).unwrap().state,
        SessionState::Idle
    );
    assert!(
        untrusted.reconcile_capability(&id, input).is_err(),
        "an exact retry still requires host authorization"
    );
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reliable_cells_value_or_digest_rejects_a_contradiction_before_core_commit() {
    for omitted in [false, true] {
        let dir = setup();
        let service = Arc::new(UnknownService(AtomicUsize::new(0)));
        let mut config = RuntimeConfig::default();
        config.host.external = Some(service.clone());
        config.permissions = vec![ScopedPermission::exact(
            "search.query",
            json!({"query":"test"}),
        )];
        let provider = Arc::new(Script {
            script: "return cayw.call('search.query',{query='test'})",
            calls: AtomicUsize::new(0),
        });
        let rt = open(&dir, provider.clone(), config.clone());
        let id = rt.create(None).unwrap().id;
        let unknown = rt
            .prompt(&id, "lose service response", CancellationToken::new())
            .await
            .unwrap();
        let call = unknown.unresolved_capabilities[0].clone();
        assert_eq!(service.0.load(Ordering::SeqCst), 1);
        // A separate trusted host inspects the actual service counter and saves
        // its original result through the real Cells settlement API. Core has
        // not received that decision. No database row is manufactured here.
        let journal =
            Arc::new(cayw_cells::SqliteJournal::open(dir.path().join("sessions.db")).unwrap());
        let identity = cayw_cells::CellIdentity {
            session_id: id.clone(),
            task_id: call.task_id.clone(),
            attempt_id: call.lease.attempt_id.clone(),
            permission_context: format!("{}:{}", call.lease.worker_id, call.lease.fence),
        };
        let scope = journal
            .stopped_scope(
                cayw_cells::ExecutorStopped {
                    identity,
                    evidence: "fixture host observed original service stopped after one invocation"
                        .into(),
                },
                config.cells.clone(),
            )
            .unwrap();
        let record = scope.records().unwrap().remove(0);
        let intent = &record.calls[0];
        let reliable = json!({"count":1});
        scope
            .settle(cayw_cells::HostSettlement {
                execution: record.execution(),
                calls: vec![cayw_cells::HostCallSettlement {
                    sequence: intent.sequence,
                    name: intent.name.clone(),
                    arguments: intent.arguments.clone(),
                    dependencies: intent.dependencies.clone(),
                    terminal: cayw_cells::HostCallTerminal::Succeeded {
                        result: if omitted {
                            cayw_cells::HostResult::Omitted {
                                sha256: cayw_cells::HostResult::value_hash(&reliable),
                            }
                        } else {
                            cayw_cells::HostResult::Value(reliable)
                        },
                    },
                }],
                execution_terminal: None,
                evidence: "trusted fixture inspected exactly one service invocation".into(),
            })
            .unwrap();
        drop(scope);
        let before = rt.store().snapshot(&id).unwrap();
        let events = rt.events(&id, 0, 1000).unwrap();
        let input = host_receipt(
            &dir,
            CapabilityReconciliation {
                key: call.key.clone(),
                expected: (&call).into(),
                actor: "fixture-host".into(),
                evidence: String::new(),
                outcome: CapabilityOutcome::Completed {
                    result: json!({"count":2}),
                },
                external_provenance: call.external_provenance.clone(),
            },
        );
        assert!(rt.reconcile_capability(&id, input).is_err());
        assert_eq!(
            rt.store().snapshot(&id).unwrap(),
            before,
            "contradiction changed Core, omitted={omitted}"
        );
        assert_eq!(rt.events(&id, 0, 1000).unwrap(), events);
        assert_eq!(service.0.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn start_persistence_failure_stops_before_effect_and_conservatively_requires_inspection() {
    let dir = setup();
    let provider = Arc::new(Script {
        script: "return cayw.call('fs.write',{path='absent.txt',content='no'})",
        calls: AtomicUsize::new(0),
    });
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"absent.txt","content":"no"}),
        )],
        ..Default::default()
    };
    let rt = open(&dir, provider.clone(), config);
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER runtime_test_reject_start BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='capability_started'
        BEGIN SELECT RAISE(ABORT,'synthetic start storage failure'); END;",
    )
    .unwrap();
    let _ = rt
        .prompt(&id, "fail durable start", CancellationToken::new())
        .await;
    assert!(!dir.path().join("workspace/absent.txt").exists());
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}
