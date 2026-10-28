//! Public model-request reconciliation with local fixtures. No live model calls.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_core::{CoreEvent, SessionStore};
use cayw_runtime::{
    ModelCostStatus, ModelRequestOrigin, ModelRequestReconciledOutcome, ModelRequestReconciliation,
    ModelRequestReconciliationVerifier, ModelRequestRecord, ModelRequestTerminal,
    ModelUsageObservation, Runtime, RuntimeConfig, RuntimeError, TaskState,
};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Semaphore, time::timeout};

const BOUND: Duration = Duration::from_secs(20);
const LIMIT: u64 = 1000;
const KIND: &str = "model_request_reconciliation";

#[derive(Default)]
struct Receipts {
    accepted: Mutex<Vec<ModelRequestReconciliation>>,
    calls: AtomicUsize,
}
impl Receipts {
    fn accept(&self, input: &ModelRequestReconciliation) {
        self.accepted.lock().unwrap().push(input.clone());
    }
}
impl ModelRequestReconciliationVerifier for Receipts {
    fn verify(&self, input: &ModelRequestReconciliation) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.accepted.lock().unwrap().contains(input) {
            Ok(())
        } else {
            Err("no trusted receipt for this exact request and decision".into())
        }
    }
}

#[derive(Default)]
struct LocalFailure {
    calls: AtomicUsize,
}
impl Provider for LocalFailure {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter([AssistantEvent::Error {
            message: AssistantMessage::new(&request.model)
                .failure(StopReason::Error, "fixture rejected before transport"),
        }]))
    }
}

struct Fixture {
    root: tempfile::TempDir,
    runtime: Runtime,
    provider: Arc<LocalFailure>,
    receipts: Arc<Receipts>,
    session: String,
    record: ModelRequestRecord,
}
fn open(
    root: &Path,
    provider: Arc<dyn Provider>,
    verifier: Option<Arc<dyn ModelRequestReconciliationVerifier>>,
    max_event_bytes: usize,
) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut model = Model::new("fixture", "request-reconciliation");
    model.max_output_tokens = Some(16);
    Runtime::open(
        root.join("sessions.db"),
        workspace,
        provider,
        model,
        RuntimeConfig {
            max_total_tokens: LIMIT,
            max_event_bytes,
            model_request_reconciliation_verifier: verifier,
            ..Default::default()
        },
    )
    .unwrap()
}
async fn failed(runtime: &Runtime, session: &str) -> ModelRequestRecord {
    timeout(
        BOUND,
        runtime.prompt(session, "local failure", CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    runtime.wait_idle(session).await;
    let ledger = runtime.model_requests(session).unwrap();
    let record = ledger.requests.last().unwrap().clone();
    assert_eq!(
        runtime.task(&record.identity.task_id).unwrap().state,
        TaskState::Failed
    );
    assert_eq!(record.terminal, Some(ModelRequestTerminal::Failed));
    assert!(record.started_cursor.is_some());
    assert_eq!(record.settled_tokens, None);
    assert_eq!(record.outstanding_reservation_tokens, LIMIT);
    record
}
async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let provider = Arc::new(LocalFailure::default());
    let runtime = open(
        root.path(),
        provider.clone(),
        Some(receipts.clone()),
        2 * 1024 * 1024,
    );
    let session = runtime.create(None).unwrap().id;
    let record = failed(&runtime, &session).await;
    Fixture {
        root,
        runtime,
        provider,
        receipts,
        session,
        record,
    }
}
fn decision(record: &ModelRequestRecord) -> ModelRequestReconciliation {
    ModelRequestReconciliation {
        expected: ModelRequestOrigin::from(record),
        actor: "trusted-local-host".into(),
        evidence: format!("fixture receipt for {}", record.identity.request_id),
        outcome: ModelRequestReconciledOutcome::NotDispatched {
            reason: "fixture proved the exact request failed before transport".into(),
        },
    }
}
fn unchanged(runtime: &Runtime, session: &str, record: &ModelRequestRecord) {
    let ledger = runtime.model_requests(session).unwrap();
    assert_eq!(
        ledger
            .requests
            .iter()
            .find(|r| r.identity == record.identity),
        Some(record)
    );
    assert!(ledger.budget_unknown);
    assert_eq!(record.outstanding_reservation_tokens, LIMIT);
}
fn save(name: &str, value: &impl serde::Serialize) {
    let Some(directory) = std::env::var_os("CAYW_MODEL_RECONCILIATION_EVIDENCE_DIR") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(format!("{name}.json")))
        .unwrap();
    serde_json::to_writer_pretty(file, value).unwrap();
}

struct CapturedResponse {
    previous: AssistantMessage,
    abort: bool,
    calls: AtomicUsize,
}
impl Provider for CapturedResponse {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let partial = self.previous.clone();
        Box::pin(futures::stream::iter([
            AssistantEvent::TextDelta {
                index: 0,
                delta: "saved response prefix".into(),
                partial: partial.clone(),
            },
            AssistantEvent::Error {
                message: partial.failure(
                    if self.abort {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    },
                    "fixture parser stopped before the captured usage tail",
                ),
            },
        ]))
    }
}

fn response_prefix() -> AssistantMessage {
    let mut message = AssistantMessage::new(&Model::new("fixture", "request-reconciliation"));
    message.content.push(Content::text("saved response prefix"));
    message.response_id = Some("captured-response".into());
    message.response_model = Some("service-model".into());
    message
}

fn complete_report() -> ModelUsageObservation {
    ModelUsageObservation {
        event: "done".into(),
        usage_status: UsageStatus::Reported,
        usage: Usage {
            input: 80,
            output: 12,
            cache_read: 3,
            cache_write: 2,
            reasoning: 7,
            total_tokens: 97,
        },
        usage_fields: serde_json::from_value(json!({
            "prompt_tokens":85,"completion_tokens":12,"total_tokens":97,
            "prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},
            "completion_tokens_details":{"reasoning_tokens":7}
        }))
        .unwrap(),
        response_id: Some("captured-response".into()),
        response_model: Some("service-model".into()),
        stop_reason: StopReason::Stop,
        metadata_redacted: false,
    }
}

fn reported_decision(record: &ModelRequestRecord) -> ModelRequestReconciliation {
    ModelRequestReconciliation {
        expected: ModelRequestOrigin::from(record),
        actor: "trusted-capture-verifier".into(),
        evidence: format!(
            "complete fixture capture and original event prefix for {}",
            record.identity.request_id
        ),
        outcome: ModelRequestReconciledOutcome::ReportedUsage {
            report: complete_report(),
        },
    }
}

fn report_mut(input: &mut ModelRequestReconciliation) -> &mut ModelUsageObservation {
    let ModelRequestReconciledOutcome::ReportedUsage { report } = &mut input.outcome else {
        panic!("reported decision expected");
    };
    report
}

struct ReportedFixture {
    root: tempfile::TempDir,
    runtime: Runtime,
    provider: Arc<CapturedResponse>,
    receipts: Arc<Receipts>,
    session: String,
    record: ModelRequestRecord,
}
async fn reported_fixture_with(previous: AssistantMessage, abort: bool) -> ReportedFixture {
    let root = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let provider = Arc::new(CapturedResponse {
        previous,
        abort,
        calls: AtomicUsize::new(0),
    });
    let runtime = open(
        root.path(),
        provider.clone(),
        Some(receipts.clone()),
        2 * 1024 * 1024,
    );
    let session = runtime.create(None).unwrap().id;
    runtime
        .prompt(
            &session,
            "captured response fixture",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    runtime.wait_idle(&session).await;
    let record = runtime.model_requests(&session).unwrap().requests[0].clone();
    assert_eq!(record.settled_tokens, None);
    assert_eq!(record.outstanding_reservation_tokens, LIMIT);
    ReportedFixture {
        root,
        runtime,
        provider,
        receipts,
        session,
        record,
    }
}
async fn reported_fixture() -> ReportedFixture {
    reported_fixture_with(response_prefix(), false).await
}

#[tokio::test]
async fn exact_decision_releases_only_its_hold_and_survives_reopen_and_retry() {
    let f = fixture().await;
    let input = decision(&f.record);
    f.receipts.accept(&input);
    let snapshot = f.runtime.store().snapshot(&f.session).unwrap();
    let tasks = f.runtime.tasks(&f.session).unwrap();
    let before = f.runtime.events(&f.session, 0, 1000).unwrap();
    let result = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap();
    let ledger = f.runtime.model_requests(&f.session).unwrap();
    let mut expected = f.record.clone();
    expected.settled_tokens = Some(0);
    expected.outstanding_reservation_tokens = 0;
    expected.reconciliation = Some(result.clone());
    assert_eq!(ledger.requests, vec![expected.clone()]);
    assert_eq!(ledger.settled_tokens, 0);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.remaining_tokens, LIMIT);
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    assert!(expected.cost_amount.is_none() && expected.cost_currency.is_none());
    assert_eq!(expected.last_cursor, f.record.last_cursor);
    assert!(result.cursor > f.record.last_cursor);
    let after = f.runtime.events(&f.session, 0, 1000).unwrap();
    assert_eq!(&after[..before.len()], &before);
    assert_eq!(after.len(), before.len() + 1);
    let event = after.last().unwrap();
    assert_eq!(event.cursor, result.cursor);
    assert!(event.attempt.is_none());
    assert!(matches!(&event.event, CoreEvent::Host {kind,data}
        if kind == KIND && data["actor"] == input.actor && data["decision"] == serde_json::to_value(&input).unwrap()));
    let mut unchanged_snapshot = snapshot.clone();
    unchanged_snapshot.cursor = result.cursor;
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap(),
        unchanged_snapshot
    );
    assert_eq!(f.runtime.tasks(&f.session).unwrap(), tasks);
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
    let retry = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap();
    assert_eq!(retry, result);
    assert_eq!(f.runtime.events(&f.session, 0, 1000).unwrap(), after);
    assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 1);
    save(
        "exact-decision",
        &json!({"input":input,"before":before,"after":after,"ledger":ledger,"tasks":tasks}),
    );
    drop(f.runtime);
    let reopened = open(f.root.path(), f.provider.clone(), None, 2 * 1024 * 1024);
    assert_eq!(reopened.model_requests(&f.session).unwrap(), ledger);
    assert_eq!(
        reopened.reconcile_model_request(input).await.unwrap(),
        result
    );
    assert_eq!(reopened.tasks(&f.session).unwrap(), tasks);
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn absent_verifier_and_untrusted_actor_or_evidence_cannot_release_a_hold() {
    let f = fixture().await;
    let input = decision(&f.record);
    let peer = open(f.root.path(), f.provider.clone(), None, 2 * 1024 * 1024);
    assert!(
        peer.reconcile_model_request(input.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("trusted host verifier")
    );
    assert!(
        f.runtime
            .reconcile_model_request(input.clone())
            .await
            .is_err()
    );
    f.receipts.accept(&input);
    let mut forged = input.clone();
    forged.actor = "actor is not a receipt".into();
    assert!(f.runtime.reconcile_model_request(forged).await.is_err());
    let mut forged = input.clone();
    forged.evidence = "no HTTP captured, trust this text".into();
    assert!(f.runtime.reconcile_model_request(forged).await.is_err());
    unchanged(&f.runtime, &f.session, &f.record);
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn every_original_identity_and_cursor_must_match_and_conflicts_are_rejected() {
    let f = fixture().await;
    let other = f.runtime.create(None).unwrap().id;
    let other_record = failed(&f.runtime, &other).await;
    let input = decision(&f.record);
    let mut candidates = Vec::new();
    macro_rules! changed {
        ($field:ident, $value:expr) => {{
            let mut value = input.clone();
            value.expected.$field = $value;
            candidates.push(value);
        }};
    }
    let mut identity = input.expected.identity.clone();
    identity.request_id = other_record.identity.request_id.clone();
    changed!(identity, identity);
    for field in 0..10 {
        let mut identity = input.expected.identity.clone();
        match field {
            0 => identity.request_id = "absent".into(),
            1 => identity.lease.session_id = other.clone(),
            2 => identity.lease.attempt_id = "other-attempt".into(),
            3 => identity.lease.worker_id = "other-worker".into(),
            4 => identity.lease.fence += 1,
            5 => identity.task_id = other_record.identity.task_id.clone(),
            6 => identity.budget_session_id = other.clone(),
            7 => identity.turn += 1,
            8 => identity.provider = "other-provider".into(),
            9 => identity.model = "other-model".into(),
            _ => unreachable!(),
        }
        changed!(identity, identity);
    }
    changed!(reservation_tokens, f.record.reservation_tokens - 1);
    changed!(reservation_cursor, None);
    changed!(
        reservation_cursor,
        Some(f.record.reservation_cursor.unwrap() + 1)
    );
    changed!(started_cursor, None);
    changed!(started_cursor, Some(f.record.started_cursor.unwrap() - 1));
    changed!(terminal, None);
    changed!(terminal, Some(ModelRequestTerminal::Cancelled));
    changed!(terminal_cursor, None);
    changed!(terminal_cursor, Some(f.record.terminal_cursor.unwrap() - 1));
    changed!(last_cursor, f.record.last_cursor - 1);
    changed!(last_cursor, f.record.last_cursor + 1);
    for candidate in candidates {
        f.receipts.accept(&candidate);
        assert!(f.runtime.reconcile_model_request(candidate).await.is_err());
        unchanged(&f.runtime, &f.session, &f.record);
    }
    assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 0);
    f.receipts.accept(&input);
    let result = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap();
    let cursor = f.runtime.store().snapshot(&f.session).unwrap().cursor;
    for field in 0..3 {
        let mut conflicting = input.clone();
        match field {
            0 => conflicting.actor.push_str(" changed"),
            1 => conflicting.evidence.push_str(" changed"),
            2 => {
                conflicting.outcome = ModelRequestReconciledOutcome::NotDispatched {
                    reason: "changed conclusion".into(),
                }
            }
            _ => unreachable!(),
        }
        f.receipts.accept(&conflicting);
        assert!(
            f.runtime
                .reconcile_model_request(conflicting)
                .await
                .is_err()
        );
    }
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap().cursor,
        cursor
    );
    assert_eq!(
        f.runtime.reconcile_model_request(input).await.unwrap(),
        result
    );
    unchanged(&f.runtime, &other, &other_record);
}

struct ResponseTrace(usize);
impl Provider for ResponseTrace {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model)
            .failure(StopReason::Error, "fixture response failed");
        match self.0 {
            0 => message.usage_status = UsageStatus::Reported,
            1 => message.usage_status = UsageStatus::Partial,
            2 => message.usage_status = UsageStatus::Invalid,
            3 => message.usage.input = 1,
            4 => {
                message.usage_fields.insert("input_tokens".into(), json!(0));
            }
            5 => message.response_id = Some("service-response".into()),
            6 => message.response_model = Some("service-model".into()),
            7 => message.content.push(Content::text("partial response")),
            8 => {
                message
                    .provider_metadata
                    .insert("opaque".into(), json!("response item"));
            }
            9 => {
                let mut partial = AssistantMessage::new(&request.model);
                partial
                    .content
                    .push(Content::text("response before empty error"));
                return Box::pin(futures::stream::iter([
                    AssistantEvent::Start { message: partial },
                    AssistantEvent::Error { message },
                ]));
            }
            _ => unreachable!(),
        }
        Box::pin(futures::stream::iter([AssistantEvent::Error { message }]))
    }
}

#[tokio::test]
async fn reported_zero_partial_invalid_usage_and_response_traces_block_non_dispatch() {
    for scenario in 0..10 {
        let root = tempfile::tempdir().unwrap();
        let receipts = Arc::new(Receipts::default());
        let runtime = open(
            root.path(),
            Arc::new(ResponseTrace(scenario)),
            Some(receipts.clone()),
            2 * 1024 * 1024,
        );
        let session = runtime.create(None).unwrap().id;
        timeout(
            BOUND,
            runtime.prompt(&session, "response trace", CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        let before = runtime.model_requests(&session).unwrap();
        let record = &before.requests[0];
        if scenario == 0 {
            assert_eq!(record.settled_tokens, Some(0));
        }
        let input = decision(record);
        receipts.accept(&input);
        assert!(
            runtime.reconcile_model_request(input).await.is_err(),
            "scenario {scenario}"
        );
        assert_eq!(runtime.model_requests(&session).unwrap(), before);
        assert_eq!(receipts.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn failed_persistence_keeps_the_hold_and_the_same_decision_can_retry() {
    let f = fixture().await;
    let input = decision(&f.record);
    f.receipts.accept(&input);
    let db = rusqlite::Connection::open(f.root.path().join("sessions.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reconciliation_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='model_request_reconciliation' BEGIN SELECT RAISE(ABORT,'fixture reconciliation persistence failure'); END;").unwrap();
    let cursor = f.runtime.store().snapshot(&f.session).unwrap().cursor;
    let error = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("fixture reconciliation persistence failure")
    );
    unchanged(&f.runtime, &f.session, &f.record);
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap().cursor,
        cursor
    );
    let peer = open(
        f.root.path(),
        f.provider.clone(),
        Some(f.receipts.clone()),
        2 * 1024 * 1024,
    );
    unchanged(&peer, &f.session, &f.record);
    save(
        "failed-persistence",
        &json!({"error":error.to_string(),"ledger":peer.model_requests(&f.session).unwrap()}),
    );
    db.execute_batch("DROP TRIGGER reconciliation_failure;")
        .unwrap();
    let result = peer.reconcile_model_request(input).await.unwrap();
    assert_eq!(result.cursor, cursor + 1);
    assert_eq!(
        f.runtime
            .model_requests(&f.session)
            .unwrap()
            .reserved_tokens,
        0
    );
}

#[tokio::test]
async fn bounded_decision_failure_does_not_release_a_hold() {
    let f = fixture().await;
    let input = decision(&f.record);
    f.receipts.accept(&input);
    let small = open(
        f.root.path(),
        f.provider.clone(),
        Some(f.receipts.clone()),
        1,
    );
    assert!(matches!(
        small.reconcile_model_request(input).await,
        Err(RuntimeError::Budget("model accounting event byte"))
    ));
    unchanged(&f.runtime, &f.session, &f.record);
}

fn task_lock(root: &Path, task: &str) -> File {
    let database = root.join("sessions.db").canonicalize().unwrap();
    let mut path = database.into_os_string();
    path.push(format!(".runtime-lock.task-{task}"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    lock.try_lock().unwrap();
    lock
}

#[tokio::test]
async fn original_executor_lock_must_be_released_before_reconciliation() {
    let f = fixture().await;
    let input = decision(&f.record);
    f.receipts.accept(&input);
    let owner = task_lock(f.root.path(), &f.record.identity.task_id);
    assert!(matches!(
        f.runtime.reconcile_model_request(input.clone()).await,
        Err(RuntimeError::Busy)
    ));
    unchanged(&f.runtime, &f.session, &f.record);
    drop(owner);
    f.runtime.reconcile_model_request(input).await.unwrap();
}

struct HeldProvider {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}
impl Provider for HeldProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(futures::stream::once(async move {
            entered.add_permits(1);
            release.acquire().await.unwrap().forget();
            AssistantEvent::Error {
                message: AssistantMessage::new(&request.model)
                    .failure(StopReason::Error, "held fixture ended"),
            }
        }))
    }
}

#[tokio::test]
async fn an_active_peer_in_the_same_budget_tree_blocks_reconciliation() {
    let f = fixture().await;
    let branch = f.runtime.fork(&f.session, "same budget", true).unwrap().id;
    let task = f.runtime.enqueue(&branch, "peer work").unwrap();
    let input = decision(&f.record);
    f.receipts.accept(&input);
    // A claimed queued task is active even before it can append an attempt.
    // Hold its execution lock in a peer, just as a running Runtime does.
    let owner = task_lock(f.root.path(), &task.id);
    let peer = open(
        f.root.path(),
        f.provider.clone(),
        Some(f.receipts.clone()),
        2 * 1024 * 1024,
    );
    assert!(matches!(
        peer.reconcile_model_request(input.clone()).await,
        Err(RuntimeError::Busy)
    ));
    unchanged(&f.runtime, &f.session, &f.record);
    drop(owner);
    let lease = peer.store().begin_attempt(&branch, "peer-worker").unwrap();
    assert!(matches!(
        f.runtime.reconcile_model_request(input.clone()).await,
        Err(RuntimeError::Busy)
    ));
    unchanged(&f.runtime, &f.session, &f.record);
    peer.store().finish_attempt(&lease).unwrap();
    f.runtime.reconcile_model_request(input).await.unwrap();
}

#[tokio::test]
async fn an_active_unrelated_budget_does_not_block_reconciliation_or_dispatch_more_work() {
    let f = fixture().await;
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let peer = open(
        f.root.path(),
        Arc::new(HeldProvider {
            entered: entered.clone(),
            release: release.clone(),
        }),
        Some(f.receipts.clone()),
        2 * 1024 * 1024,
    );
    let unrelated = peer.create(None).unwrap().id;
    let running = tokio::spawn(async move {
        peer.prompt(&unrelated, "hold", CancellationToken::new())
            .await
    });
    timeout(BOUND, entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let input = decision(&f.record);
    f.receipts.accept(&input);
    f.runtime.reconcile_model_request(input).await.unwrap();
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    timeout(BOUND, running).await.unwrap().unwrap().unwrap();
}

fn synthetic_hold(runtime: &Runtime, session: &str, budget: &str) -> ModelRequestRecord {
    // Simulate a crash-era peer journal through the public Core API. The
    // reconciliation target still comes from a real failed Runtime task.
    let lease = runtime
        .store()
        .begin_attempt(session, "synthetic-old-peer")
        .unwrap();
    let identity = json!({
        "request_id": uuid::Uuid::new_v4().to_string(),
        "lease":lease,
        "task_id":uuid::Uuid::new_v4().to_string(),
        "budget_session_id":budget,
        "turn":1,
        "provider":"fixture",
        "model":"request-reconciliation"
    });
    let reserved = runtime
        .store()
        .append_host_event(
            &lease,
            "model_request",
            json!({
                "record":"reserved","identity":identity,"policy":"entire_session_remainder",
                "tokens":LIMIT,"token_limit":LIMIT,"output_max_tokens":16
            }),
        )
        .unwrap();
    let started = runtime
        .store()
        .append_host_event(
            &lease,
            "model_request",
            json!({
                "record":"started","request_id":identity["request_id"],
                "reservation_cursor":reserved.cursor,"previous_cursor":reserved.cursor
            }),
        )
        .unwrap();
    runtime
        .store()
        .append_host_event(
            &lease,
            "model_request",
            json!({
                "record":"finished","request_id":identity["request_id"],
                "reservation_cursor":reserved.cursor,"previous_cursor":started.cursor,
                "terminal":"failed","settled_tokens":null,"last_received":null
            }),
        )
        .unwrap();
    runtime.store().finish_attempt(&lease).unwrap();
    runtime
        .model_requests(session)
        .unwrap()
        .requests
        .into_iter()
        .find(|r| r.identity.request_id == identity["request_id"])
        .unwrap()
}

#[tokio::test]
async fn reconciliation_preserves_other_unknown_requests_in_the_same_budget_tree() {
    let f = fixture().await;
    let branch = f
        .runtime
        .fork(&f.session, "old peer budget", true)
        .unwrap()
        .id;
    let other = synthetic_hold(&f.runtime, &branch, &f.session);
    let before = f.runtime.model_requests(&f.session).unwrap();
    assert_eq!(before.reserved_tokens, LIMIT * 2);
    let input = decision(&f.record);
    f.receipts.accept(&input);
    let result = f.runtime.reconcile_model_request(input).await.unwrap();
    let after = f.runtime.model_requests(&branch).unwrap();
    assert_eq!(after.reserved_tokens, LIMIT);
    assert_eq!(after.settled_tokens, 0);
    assert!(after.budget_unknown);
    assert_eq!(
        after.requests.iter().find(|r| r.identity == other.identity),
        Some(&other)
    );
    assert_eq!(
        after
            .requests
            .iter()
            .find(|r| r.identity == f.record.identity)
            .unwrap()
            .reconciliation
            .as_ref(),
        Some(&result)
    );
    let queued = f.runtime.enqueue(&branch, "still refused").unwrap();
    f.runtime
        .resume(&queued.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
    let refused = f.runtime.model_requests(&branch).unwrap();
    assert_eq!(refused.reserved_tokens, LIMIT);
    assert_eq!(
        refused
            .requests
            .iter()
            .find(|r| r.identity == other.identity),
        Some(&other)
    );
    assert!(
        refused
            .requests
            .iter()
            .any(|r| r.identity.task_id == queued.id
                && r.rejection.as_deref() == Some("unsettled_model_reservation"))
    );
    save(
        "other-unknown",
        &json!({"before":before,"after":after,"after_refusal":refused}),
    );
}

struct ChangeCursor {
    path: std::path::PathBuf,
    start_attempt: bool,
}
impl ModelRequestReconciliationVerifier for ChangeCursor {
    fn verify(&self, input: &ModelRequestReconciliation) -> Result<(), String> {
        // A separate trusted Core writer bypasses the Runtime lifecycle lock.
        // The final control-event CAS must still refuse stale session facts.
        let store = SessionStore::open(&self.path).map_err(|e| e.to_string())?;
        let id = &input.expected.identity.lease.session_id;
        if self.start_attempt {
            store
                .begin_attempt(id, "external-core-writer")
                .map_err(|e| e.to_string())?;
        } else {
            let snapshot = store.snapshot(id).map_err(|e| e.to_string())?;
            store
                .append_control_event(
                    id,
                    snapshot.cursor,
                    None,
                    "external-core-writer",
                    "fixture_cursor_changed",
                    json!({}),
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_cursor_or_attempt_change_during_verification_cannot_release_the_hold() {
    for start_attempt in [false, true] {
        let f = fixture().await;
        let peer = open(
            f.root.path(),
            f.provider.clone(),
            Some(Arc::new(ChangeCursor {
                path: f.root.path().join("sessions.db"),
                start_attempt,
            })),
            2 * 1024 * 1024,
        );
        let error = peer
            .reconcile_model_request(decision(&f.record))
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::Core(_)));
        unchanged(&f.runtime, &f.session, &f.record);
        assert!(
            !f.runtime
                .events(&f.session, 0, 1000)
                .unwrap()
                .iter()
                .any(|e| matches!(&e.event, CoreEvent::Host {kind,..} if kind == KIND))
        );
        save(
            if start_attempt {
                "attempt-cas"
            } else {
                "cursor-cas"
            },
            &json!({"error":error.to_string(),"ledger":f.runtime.model_requests(&f.session).unwrap()}),
        );
    }
}

#[tokio::test]
async fn concurrent_exact_retries_append_only_one_decision() {
    let f = fixture().await;
    let input = decision(&f.record);
    f.receipts.accept(&input);
    let peer = open(
        f.root.path(),
        f.provider.clone(),
        Some(f.receipts.clone()),
        2 * 1024 * 1024,
    );
    let cursor = f.runtime.store().snapshot(&f.session).unwrap().cursor;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let first = f.runtime.clone();
    let first_input = input.clone();
    let first_barrier = barrier.clone();
    let first = tokio::task::spawn_blocking(move || {
        first_barrier.wait();
        futures::executor::block_on(first.reconcile_model_request(first_input))
    });
    let second = tokio::task::spawn_blocking(move || {
        barrier.wait();
        futures::executor::block_on(peer.reconcile_model_request(input))
    });
    let (a, b) = timeout(BOUND, async { tokio::join!(first, second) })
        .await
        .unwrap();
    assert_eq!(a.unwrap().unwrap(), b.unwrap().unwrap());
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap().cursor,
        cursor + 1
    );
    assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_decision_serialization_rejects_unknown_fields_and_outcomes() {
    let f = fixture().await;
    let input = decision(&f.record);
    let value = serde_json::to_value(&input).unwrap();
    assert_eq!(value["outcome"]["type"], "not_dispatched");
    assert_eq!(
        serde_json::from_value::<ModelRequestReconciliation>(value.clone()).unwrap(),
        input
    );
    let mut unknown_field = value.clone();
    unknown_field["outcome"]["settled_tokens"] = json!(0);
    assert!(serde_json::from_value::<ModelRequestReconciliation>(unknown_field).is_err());
    let mut unknown_outcome = value;
    unknown_outcome["outcome"]["type"] = json!("unsupported_outcome");
    assert!(serde_json::from_value::<ModelRequestReconciliation>(unknown_outcome).is_err());
}

#[tokio::test]
async fn reported_usage_settles_a_failed_response_without_replacing_any_original_fact() {
    let f = reported_fixture().await;
    assert_eq!(f.record.terminal, Some(ModelRequestTerminal::Failed));
    let input = reported_decision(&f.record);
    f.receipts.accept(&input);
    let before = f.runtime.events(&f.session, 0, 1000).unwrap();
    let snapshot = f.runtime.store().snapshot(&f.session).unwrap();
    let tasks = f.runtime.tasks(&f.session).unwrap();
    let result = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap();
    let ledger = f.runtime.model_requests(&f.session).unwrap();
    let mut expected = f.record.clone();
    expected.settled_tokens = Some(97);
    expected.outstanding_reservation_tokens = 0;
    expected.reconciliation = Some(result.clone());
    assert_eq!(ledger.requests, vec![expected]);
    assert_eq!(ledger.settled_tokens, 97);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.remaining_tokens, LIMIT - 97);
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    let after = f.runtime.events(&f.session, 0, 1000).unwrap();
    assert_eq!(&after[..before.len()], &before);
    assert_eq!(after.len(), before.len() + 1);
    let mut expected_snapshot = snapshot.clone();
    expected_snapshot.cursor = result.cursor;
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap(),
        expected_snapshot
    );
    assert_eq!(
        f.runtime.get(&f.session).unwrap().messages,
        snapshot.messages
    );
    assert_eq!(f.runtime.tasks(&f.session).unwrap(), tasks);
    assert_eq!(tasks[0].state, TaskState::Failed);
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.runtime
            .reconcile_model_request(input.clone())
            .await
            .unwrap(),
        result
    );
    assert_eq!(f.runtime.events(&f.session, 0, 1000).unwrap(), after);
    assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 1);
    save(
        "reported-failed",
        &json!({"input":input,"before":before,"after":after,"ledger":ledger,"tasks":tasks}),
    );
    drop(f.runtime);
    let reopened = open(f.root.path(), f.provider.clone(), None, 2 * 1024 * 1024);
    assert_eq!(reopened.model_requests(&f.session).unwrap(), ledger);
    assert_eq!(
        reopened.reconcile_model_request(input).await.unwrap(),
        result
    );
    assert_eq!(reopened.tasks(&f.session).unwrap(), tasks);
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
}

const CAPTURE_LIMIT: u64 = 726_901;

fn cache_creation_report() -> ModelUsageObservation {
    let mut report = complete_report();
    report.usage = Usage {
        input: 536,
        output: 1479,
        cache_read: 34_560,
        cache_write: 0,
        reasoning: 0,
        total_tokens: 36_575,
    };
    report.usage_fields = serde_json::from_value(json!({
        "prompt_tokens":35_096,"completion_tokens":1479,"total_tokens":36_575,
        "prompt_tokens_details":{
            "audio_tokens":0,"cache_creation_input_tokens":0,
            "cache_write_tokens":0,"cached_tokens":34_560
        },
        "completion_tokens_details":{
            "accepted_prediction_tokens":0,"audio_tokens":0,
            "reasoning_tokens":0,"rejected_prediction_tokens":0
        }
    }))
    .unwrap();
    report.stop_reason = StopReason::ToolUse;
    report
}

struct InterruptedCapture {
    calls: AtomicUsize,
}
impl Provider for InterruptedCapture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let report = cache_creation_report();
        let mut partial = AssistantMessage::new(&request.model);
        partial.content.push(Content::text("saved response prefix"));
        partial.response_id = report.response_id;
        partial.response_model = report.response_model;
        partial.usage_status = report.usage_status;
        partial.usage = report.usage;
        partial.usage_fields = report.usage_fields;
        Box::pin(futures::StreamExt::chain(
            futures::stream::iter([AssistantEvent::TextEnd { index: 0, partial }]),
            futures::stream::pending(),
        ))
    }
}

fn open_interrupted_capture(
    root: &Path,
    provider: Arc<InterruptedCapture>,
    verifier: Option<Arc<dyn ModelRequestReconciliationVerifier>>,
) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut model = Model::new("fixture", "request-reconciliation");
    model.max_output_tokens = Some(8192);
    Runtime::open(
        root.join("sessions.db"),
        workspace,
        provider,
        model,
        RuntimeConfig {
            max_total_tokens: CAPTURE_LIMIT,
            max_run_time: Duration::from_secs(1),
            model_request_reconciliation_verifier: verifier,
            ..Default::default()
        },
    )
    .unwrap()
}

async fn interrupted_capture() -> (
    tempfile::TempDir,
    Runtime,
    Arc<InterruptedCapture>,
    Arc<Receipts>,
    String,
    ModelRequestRecord,
) {
    let root = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let provider = Arc::new(InterruptedCapture {
        calls: AtomicUsize::new(0),
    });
    let runtime = open_interrupted_capture(root.path(), provider.clone(), Some(receipts.clone()));
    let session = runtime.create(None).unwrap().id;
    assert!(matches!(
        runtime
            .prompt(&session, "interrupted capture", CancellationToken::new())
            .await,
        Err(RuntimeError::Budget("time"))
    ));
    runtime.wait_idle(&session).await;
    let record = runtime.model_requests(&session).unwrap().requests[0].clone();
    assert_eq!(record.terminal, Some(ModelRequestTerminal::Dropped));
    assert_eq!(record.settled_tokens, None);
    assert_eq!(record.outstanding_reservation_tokens, CAPTURE_LIMIT);
    assert_eq!(record.observations.len(), 1);
    assert_eq!(
        record.observations[0].observation.usage_fields,
        cache_creation_report().usage_fields
    );
    assert_eq!(
        runtime.task(&record.identity.task_id).unwrap().state,
        TaskState::Failed
    );
    (root, runtime, provider, receipts, session, record)
}

#[tokio::test(start_paused = true)]
async fn cache_creation_detail_zero_settles_once_and_preserves_dropped_observations() {
    let (root, runtime, provider, receipts, session, record) = interrupted_capture().await;
    let mut input = reported_decision(&record);
    *report_mut(&mut input) = cache_creation_report();
    receipts.accept(&input);
    let before = runtime.events(&session, 0, 1000).unwrap();
    let snapshot = runtime.store().snapshot(&session).unwrap();
    let tasks = runtime.tasks(&session).unwrap();
    let result = runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap();
    let ledger = runtime.model_requests(&session).unwrap();
    let mut expected = record.clone();
    expected.settled_tokens = Some(36_575);
    expected.outstanding_reservation_tokens = 0;
    expected.reconciliation = Some(result.clone());
    assert_eq!(ledger.requests, vec![expected]);
    assert_eq!(ledger.settled_tokens, 36_575);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.remaining_tokens, CAPTURE_LIMIT - 36_575);
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    let after = runtime.events(&session, 0, 1000).unwrap();
    assert_eq!(&after[..before.len()], &before);
    assert_eq!(after.len(), before.len() + 1);
    assert!(
        matches!(&after.last().unwrap().event, CoreEvent::Host {kind,data}
        if kind == KIND && data["decision"] == serde_json::to_value(&input).unwrap())
    );
    let mut expected_snapshot = snapshot;
    expected_snapshot.cursor = result.cursor;
    assert_eq!(
        runtime.store().snapshot(&session).unwrap(),
        expected_snapshot
    );
    assert_eq!(runtime.tasks(&session).unwrap(), tasks);
    assert_eq!(
        runtime
            .reconcile_model_request(input.clone())
            .await
            .unwrap(),
        result
    );
    assert_eq!(runtime.events(&session, 0, 1000).unwrap(), after);
    assert_eq!(receipts.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    save(
        "cache-creation-zero-dropped",
        &json!({"input":input,"before":before,"after":after,"ledger":ledger,"tasks":tasks}),
    );
    drop(runtime);
    let reopened = open_interrupted_capture(root.path(), provider.clone(), None);
    assert_eq!(reopened.model_requests(&session).unwrap(), ledger);
    assert_eq!(reopened.tasks(&session).unwrap(), tasks);
    assert_eq!(reopened.events(&session, 0, 1000).unwrap(), after);
    assert_eq!(
        reopened.reconcile_model_request(input).await.unwrap(),
        result
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cache_creation_detail_missing_nonzero_unknown_and_excess_reports_keep_the_hold() {
    let (_root, runtime, provider, receipts, session, record) = interrupted_capture().await;
    let before = runtime.events(&session, 0, 1000).unwrap();
    let ledger = runtime.model_requests(&session).unwrap();
    let mut rejected = Vec::new();
    for scenario in 0..12 {
        let mut input = reported_decision(&record);
        let report = report_mut(&mut input);
        *report = cache_creation_report();
        let details = report
            .usage_fields
            .get_mut("prompt_tokens_details")
            .unwrap();
        match scenario {
            0 => {
                details
                    .as_object_mut()
                    .unwrap()
                    .remove("cache_creation_input_tokens");
            }
            1 => details["cache_creation_input_tokens"] = json!(1),
            2 => {
                let details = details.as_object_mut().unwrap();
                details.remove("cache_creation_input_tokens");
                details.insert("unknown_counter".into(), json!(0));
            }
            3 => details["unknown_counter"] = json!(0),
            4 => details["cache_creation_input_tokens"] = json!("0"),
            5 => details["cache_creation_input_tokens"] = json!(-1),
            6 => details["cache_creation_input_tokens"] = json!(0.5),
            7 => details["cache_creation_input_tokens"] = json!(false),
            8 => details["cache_creation_input_tokens"] = json!(null),
            9 => {
                report.usage.input = CAPTURE_LIMIT - report.usage.cache_read;
                report.usage.total_tokens = CAPTURE_LIMIT + report.usage.output;
                report
                    .usage_fields
                    .insert("prompt_tokens".into(), json!(CAPTURE_LIMIT));
                report
                    .usage_fields
                    .insert("total_tokens".into(), json!(report.usage.total_tokens));
            }
            10 => {
                report.usage.output = 8193;
                report.usage.total_tokens = 35_096 + 8193;
                report
                    .usage_fields
                    .insert("completion_tokens".into(), json!(8193));
                report
                    .usage_fields
                    .insert("total_tokens".into(), json!(report.usage.total_tokens));
            }
            11 => details["cache_write_tokens"] = json!(1),
            _ => unreachable!(),
        }
        receipts.accept(&input);
        let error = runtime
            .reconcile_model_request(input.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "scenario {scenario}: {error}"
        );
        assert_eq!(
            runtime.model_requests(&session).unwrap(),
            ledger,
            "scenario {scenario}"
        );
        assert_eq!(
            runtime.events(&session, 0, 1000).unwrap(),
            before,
            "scenario {scenario}"
        );
        rejected.push(json!({"scenario":scenario,"input":input,"error":error.to_string()}));
    }
    assert_eq!(receipts.calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    save(
        "cache-creation-rejections",
        &json!({"rejected":rejected,"ledger":ledger}),
    );
}

#[tokio::test]
async fn reported_usage_still_requires_exact_origin_and_rejects_conflicting_decisions() {
    let f = reported_fixture().await;
    let input = reported_decision(&f.record);
    let origin = serde_json::to_value(&input.expected).unwrap();
    for pointer in [
        "/identity/request_id",
        "/identity/lease/session_id",
        "/identity/lease/attempt_id",
        "/identity/lease/worker_id",
        "/identity/lease/fence",
        "/identity/task_id",
        "/identity/budget_session_id",
        "/identity/turn",
        "/identity/provider",
        "/identity/model",
        "/reservation_tokens",
        "/reservation_cursor",
        "/started_cursor",
        "/terminal",
        "/terminal_cursor",
        "/last_cursor",
    ] {
        let mut changed = origin.clone();
        let field = changed.pointer_mut(pointer).unwrap();
        *field = if let Some(value) = field.as_u64() {
            json!(value + 1)
        } else if pointer == "/terminal" {
            json!("dropped")
        } else {
            json!("different-original")
        };
        let mut candidate = input.clone();
        candidate.expected = serde_json::from_value(changed).unwrap();
        f.receipts.accept(&candidate);
        assert!(
            f.runtime.reconcile_model_request(candidate).await.is_err(),
            "{pointer}"
        );
        unchanged(&f.runtime, &f.session, &f.record);
    }
    f.receipts.accept(&input);
    let result = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap();
    let cursor = f.runtime.store().snapshot(&f.session).unwrap().cursor;
    for field in 0..5 {
        let mut conflict = input.clone();
        match field {
            0 => conflict.actor.push_str(" other"),
            1 => conflict.evidence.push_str(" other"),
            2 => report_mut(&mut conflict).response_id = Some("other-response".into()),
            3 => report_mut(&mut conflict).stop_reason = StopReason::Length,
            4 => {
                conflict.outcome = ModelRequestReconciledOutcome::NotDispatched {
                    reason: "changed conclusion".into(),
                }
            }
            _ => unreachable!(),
        }
        f.receipts.accept(&conflict);
        assert!(f.runtime.reconcile_model_request(conflict).await.is_err());
    }
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap().cursor,
        cursor
    );
    assert_eq!(
        f.runtime.reconcile_model_request(input).await.unwrap(),
        result
    );
}

#[tokio::test]
async fn invalid_incomplete_or_excess_reported_usage_cannot_release_a_hold() {
    let f = reported_fixture().await;
    for scenario in 0..27 {
        let mut input = reported_decision(&f.record);
        let report = report_mut(&mut input);
        match scenario {
            0 => report.usage_status = UsageStatus::Unknown,
            1 => report.usage_status = UsageStatus::Partial,
            2 => report.usage_status = UsageStatus::Invalid,
            3 => report.event = "usage_update".into(),
            4 => report.stop_reason = StopReason::Pending,
            5 => report.stop_reason = StopReason::Aborted,
            6 => report.metadata_redacted = true,
            7 => report.response_id = None,
            8 => report.response_model = Some("".into()),
            9 => report.response_id = Some("different-response".into()),
            10 => report.response_model = Some("different-model".into()),
            11 => report.usage.total_tokens -= 1,
            12 => report.usage.reasoning = report.usage.output + 1,
            13 => report.usage.input = u64::MAX,
            14 => {
                report.usage_fields.remove("prompt_tokens");
            }
            15 => {
                report
                    .usage_fields
                    .insert("completion_tokens".into(), json!(-1));
            }
            16 => {
                report.usage_fields.insert("total_tokens".into(), json!(96));
            }
            17 => {
                report.usage_fields.insert("cost".into(), json!(0));
            }
            18 => {
                report
                    .usage_fields
                    .insert("prompt_tokens_details".into(), json!({"cached_tokens":"3"}));
            }
            19 => {
                report.usage = Usage {
                    input: 1001,
                    output: 12,
                    total_tokens: 1013,
                    ..Default::default()
                };
                report.usage_fields = serde_json::from_value(
                    json!({"prompt_tokens":1001,"completion_tokens":12,"total_tokens":1013}),
                )
                .unwrap();
            }
            20 => {
                report.usage = Usage {
                    input: 1,
                    output: 17,
                    total_tokens: 18,
                    ..Default::default()
                };
                report.usage_fields = serde_json::from_value(
                    json!({"prompt_tokens":1,"completion_tokens":17,"total_tokens":18}),
                )
                .unwrap();
            }
            21 => {
                report.usage_fields.insert("input_tokens".into(), json!(80));
            }
            22 => report.response_id = Some("control\nlabel".into()),
            23 => report.response_model = Some("x".repeat(257)),
            24 => {
                report
                    .usage_fields
                    .insert("completion_tokens".into(), json!(1.5));
            }
            25 => {
                report.usage_fields.insert(
                    "prompt_tokens_details".into(),
                    json!({"cached_tokens":86,"cache_write_tokens":2}),
                );
            }
            26 => {
                report.usage_fields.insert(
                    "completion_tokens_details".into(),
                    json!({"reasoning_tokens":13}),
                );
            }
            _ => unreachable!(),
        }
        f.receipts.accept(&input);
        assert!(
            f.runtime.reconcile_model_request(input).await.is_err(),
            "scenario {scenario}"
        );
        unchanged(&f.runtime, &f.session, &f.record);
    }
    assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prior_usage_and_agent_response_evidence_cannot_be_contradicted() {
    for scenario in 0..8 {
        let mut prefix = response_prefix();
        match scenario {
            0 => {
                prefix.usage.input = 100;
                prefix.usage.total_tokens = 100;
            }
            1 => prefix.usage.output = 13,
            2 => prefix.usage.reasoning = 8,
            3 => {
                prefix.usage_status = UsageStatus::Partial;
                prefix
                    .usage_fields
                    .insert("prompt_tokens".into(), json!(86));
            }
            4 => {
                prefix.usage_status = UsageStatus::Partial;
                prefix
                    .usage_fields
                    .insert("prompt_tokens_details".into(), json!({"cached_tokens":4}));
            }
            5 => prefix.usage_status = UsageStatus::Invalid,
            6 => prefix.stop_reason = StopReason::Length,
            7 => {
                prefix
                    .usage_fields
                    .insert("total_tokens".into(), json!("malformed"));
            }
            _ => unreachable!(),
        }
        let f = reported_fixture_with(prefix, false).await;
        let input = reported_decision(&f.record);
        f.receipts.accept(&input);
        assert!(
            f.runtime.reconcile_model_request(input).await.is_err(),
            "scenario {scenario}"
        );
        unchanged(&f.runtime, &f.session, &f.record);
        assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn prior_partial_counters_allow_cache_reclassification_but_require_complete_receipts() {
    let mut prefix = response_prefix();
    prefix.usage_status = UsageStatus::Partial;
    prefix.usage.input = 85;
    prefix.usage.total_tokens = 85;
    prefix
        .usage_fields
        .insert("prompt_tokens".into(), json!(85));
    let f = reported_fixture_with(prefix, false).await;
    let input = reported_decision(&f.record);
    let no_verifier = open(f.root.path(), f.provider.clone(), None, 2 * 1024 * 1024);
    assert!(
        no_verifier
            .reconcile_model_request(input.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("trusted host verifier")
    );
    assert!(
        f.runtime
            .reconcile_model_request(input.clone())
            .await
            .is_err()
    );
    unchanged(&f.runtime, &f.session, &f.record);
    f.receipts.accept(&input);
    f.runtime.reconcile_model_request(input).await.unwrap();
    assert_eq!(
        f.runtime.model_requests(&f.session).unwrap().settled_tokens,
        97
    );
}

#[tokio::test]
async fn cancelled_response_is_not_a_complete_capture_and_zero_defaults_do_not_settle_it() {
    let f = reported_fixture_with(response_prefix(), true).await;
    assert_eq!(f.record.terminal, Some(ModelRequestTerminal::Cancelled));
    let input = reported_decision(&f.record);
    assert!(
        f.runtime
            .reconcile_model_request(input.clone())
            .await
            .is_err()
    );
    let mut zero_default = input.clone();
    let report = report_mut(&mut zero_default);
    report.usage = Usage::default();
    report.usage_fields.clear();
    f.receipts.accept(&zero_default);
    assert!(
        f.runtime
            .reconcile_model_request(zero_default)
            .await
            .is_err()
    );
    unchanged(&f.runtime, &f.session, &f.record);
    // Only the trusted receipt for a complete captured final report permits this.
    f.receipts.accept(&input);
    let result = f.runtime.reconcile_model_request(input).await.unwrap();
    let record = &f.runtime.model_requests(&f.session).unwrap().requests[0];
    assert_eq!(record.terminal, Some(ModelRequestTerminal::Cancelled));
    assert_eq!(record.observations, f.record.observations);
    assert_eq!(record.settled_tokens, Some(97));
    assert_eq!(record.reconciliation.as_ref(), Some(&result));
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reported_usage_write_failure_and_owner_lock_keep_the_original_hold() {
    let f = reported_fixture().await;
    let input = reported_decision(&f.record);
    f.receipts.accept(&input);
    let owner = task_lock(f.root.path(), &f.record.identity.task_id);
    assert!(matches!(
        f.runtime.reconcile_model_request(input.clone()).await,
        Err(RuntimeError::Busy)
    ));
    drop(owner);
    let db = rusqlite::Connection::open(f.root.path().join("sessions.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reported_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='model_request_reconciliation' BEGIN SELECT RAISE(ABORT,'fixture reported usage write failure'); END;").unwrap();
    let before = f.runtime.events(&f.session, 0, 1000).unwrap();
    let error = f
        .runtime
        .reconcile_model_request(input.clone())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("fixture reported usage write failure")
    );
    unchanged(&f.runtime, &f.session, &f.record);
    assert_eq!(f.runtime.events(&f.session, 0, 1000).unwrap(), before);
    let peer = open(
        f.root.path(),
        f.provider.clone(),
        Some(f.receipts.clone()),
        2 * 1024 * 1024,
    );
    unchanged(&peer, &f.session, &f.record);
    save(
        "reported-write-failure",
        &json!({"error":error.to_string(),"ledger":peer.model_requests(&f.session).unwrap()}),
    );
    db.execute_batch("DROP TRIGGER reported_failure;").unwrap();
    peer.reconcile_model_request(input).await.unwrap();
    assert_eq!(
        f.runtime.model_requests(&f.session).unwrap().settled_tokens,
        97
    );
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reported_usage_keeps_other_unknowns_and_checks_the_shared_budget_tree() {
    let f = reported_fixture().await;
    let branch = f.runtime.fork(&f.session, "same budget", true).unwrap().id;
    let task = f.runtime.enqueue(&branch, "not running").unwrap();
    let input = reported_decision(&f.record);
    f.receipts.accept(&input);
    let owner = task_lock(f.root.path(), &task.id);
    assert!(matches!(
        f.runtime.reconcile_model_request(input.clone()).await,
        Err(RuntimeError::Busy)
    ));
    drop(owner);
    let other = synthetic_hold(&f.runtime, &branch, &f.session);
    f.runtime.reconcile_model_request(input).await.unwrap();
    let ledger = f.runtime.model_requests(&branch).unwrap();
    assert_eq!(
        ledger
            .requests
            .iter()
            .find(|record| record.identity == other.identity),
        Some(&other)
    );
    assert_eq!(ledger.settled_tokens, 97);
    assert_eq!(ledger.reserved_tokens, LIMIT);
    assert!(ledger.budget_unknown);
    assert_eq!(f.runtime.task(&task.id).unwrap(), task);
    assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
    save("reported-other-unknown", &ledger);
}

#[tokio::test]
async fn reported_usage_cas_failure_and_concurrent_retries_never_double_settle() {
    for start_attempt in [false, true] {
        let f = reported_fixture().await;
        let peer = open(
            f.root.path(),
            f.provider.clone(),
            Some(Arc::new(ChangeCursor {
                path: f.root.path().join("sessions.db"),
                start_attempt,
            })),
            2 * 1024 * 1024,
        );
        assert!(matches!(
            peer.reconcile_model_request(reported_decision(&f.record))
                .await,
            Err(RuntimeError::Core(_))
        ));
        unchanged(&f.runtime, &f.session, &f.record);
    }
    let f = reported_fixture().await;
    let input = reported_decision(&f.record);
    f.receipts.accept(&input);
    let peer = open(
        f.root.path(),
        f.provider.clone(),
        Some(f.receipts.clone()),
        2 * 1024 * 1024,
    );
    let cursor = f.runtime.store().snapshot(&f.session).unwrap().cursor;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let first_runtime = f.runtime.clone();
    let first_input = input.clone();
    let first_barrier = barrier.clone();
    let first = tokio::task::spawn_blocking(move || {
        first_barrier.wait();
        futures::executor::block_on(first_runtime.reconcile_model_request(first_input))
    });
    let second = tokio::task::spawn_blocking(move || {
        barrier.wait();
        futures::executor::block_on(peer.reconcile_model_request(input))
    });
    let (first, second) = timeout(BOUND, async { tokio::join!(first, second) })
        .await
        .unwrap();
    assert_eq!(first.unwrap().unwrap(), second.unwrap().unwrap());
    assert_eq!(
        f.runtime.store().snapshot(&f.session).unwrap().cursor,
        cursor + 1
    );
    assert_eq!(
        f.runtime.model_requests(&f.session).unwrap().settled_tokens,
        97
    );
    assert_eq!(f.receipts.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reported_usage_checks_each_supported_protocol_without_replaying_a_failure() {
    for (usage, fields, reason) in [
        (
            Usage {
                input: 80,
                output: 12,
                cache_read: 3,
                cache_write: 2,
                total_tokens: 97,
                ..Default::default()
            },
            json!({"input_tokens":80,"output_tokens":12,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}),
            StopReason::Error,
        ),
        (
            complete_report().usage,
            json!({"input_tokens":85,"output_tokens":12,"total_tokens":97,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":7}}),
            StopReason::Length,
        ),
        (
            Usage {
                input: 87,
                output: 12,
                cache_read: 3,
                reasoning: 7,
                total_tokens: 102,
                ..Default::default()
            },
            json!({"promptTokenCount":85,"candidatesTokenCount":5,"cachedContentTokenCount":3,"thoughtsTokenCount":7,"toolUsePromptTokenCount":5,"totalTokenCount":102}),
            StopReason::ToolUse,
        ),
    ] {
        let f = reported_fixture().await;
        let mut input = reported_decision(&f.record);
        let report = report_mut(&mut input);
        report.usage = usage.clone();
        report.usage_fields = serde_json::from_value(fields).unwrap();
        report.stop_reason = reason;
        if reason == StopReason::Error {
            report.event = "error".into();
        }
        f.receipts.accept(&input);
        f.runtime.reconcile_model_request(input).await.unwrap();
        let record = &f.runtime.model_requests(&f.session).unwrap().requests[0];
        assert_eq!(record.settled_tokens, Some(usage.total_tokens));
        assert_eq!(record.terminal, Some(ModelRequestTerminal::Failed));
        assert_eq!(f.provider.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn reported_usage_serialization_keeps_explicit_zeroes_and_rejects_unknown_fields() {
    let f = reported_fixture().await;
    let mut input = reported_decision(&f.record);
    let report = report_mut(&mut input);
    report.usage = Usage::default();
    report.usage_fields =
        serde_json::from_value(json!({"input_tokens":0,"output_tokens":0})).unwrap();
    let value = serde_json::to_value(&input).unwrap();
    assert_eq!(value["outcome"]["type"], "reported_usage");
    assert_eq!(
        serde_json::from_value::<ModelRequestReconciliation>(value.clone()).unwrap(),
        input
    );
    let mut unknown = value.clone();
    unknown["outcome"]["report"]["local_success"] = json!(true);
    assert!(serde_json::from_value::<ModelRequestReconciliation>(unknown).is_err());
    let mut missing = value;
    missing["outcome"]["report"]
        .as_object_mut()
        .unwrap()
        .remove("usage_status");
    assert!(serde_json::from_value::<ModelRequestReconciliation>(missing).is_err());
    f.receipts.accept(&input);
    f.runtime.reconcile_model_request(input).await.unwrap();
    assert_eq!(
        f.runtime.model_requests(&f.session).unwrap().settled_tokens,
        0
    );
}
