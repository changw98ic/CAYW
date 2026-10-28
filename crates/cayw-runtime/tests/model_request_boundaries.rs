//! Fixed local model responses and synthetic persisted boundary records. No live API.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_core::{AttemptLease, CoreEvent};
use cayw_runtime::{
    ModelCostStatus, ModelRequestLedger, ModelRequestTerminal, Runtime, RuntimeConfig,
    RuntimeError, TaskState,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

fn model() -> Model {
    let mut model = Model::new("fixture", "model-ledger-boundary");
    model.max_output_tokens = Some(16);
    model
}
fn config() -> RuntimeConfig {
    RuntimeConfig {
        max_total_tokens: 1000,
        ..Default::default()
    }
}
fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    root
}
fn open(root: &Path, provider: Arc<dyn Provider>) -> Runtime {
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        provider,
        model(),
        config(),
    )
    .unwrap()
}
fn answer(request: &ModelRequest, reported: bool) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.content.push(Content::text("fixed local answer"));
    message.stop_reason = StopReason::Stop;
    if reported {
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
    }
    message
}
#[derive(Default)]
struct Fixed(AtomicUsize);
impl Provider for Fixed {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: answer(&request, true),
        }]))
    }
}
struct FailedThenDone {
    calls: AtomicUsize,
    status: UsageStatus,
    invalid_total: bool,
}
impl Provider for FailedThenDone {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let mut message = answer(&request, true);
        let event = if first {
            message.usage_status = self.status;
            if self.invalid_total {
                message.usage.total_tokens += 1;
            }
            AssistantEvent::Error {
                message: message.failure(StopReason::Error, "fixed protocol failure"),
            }
        } else {
            AssistantEvent::Done { message }
        };
        Box::pin(futures::stream::iter([event]))
    }
}

#[tokio::test]
async fn reported_error_usage_settles_without_completing_or_replaying_the_failed_task() {
    let root = root();
    let provider = Arc::new(FailedThenDone {
        calls: AtomicUsize::new(0),
        status: UsageStatus::Reported,
        invalid_total: false,
    });
    let rt = open(root.path(), provider.clone());
    let id = rt.create(None).unwrap().id;
    prompt(&rt, &id).await;
    let failed = rt.tasks(&id).unwrap().pop().unwrap();
    assert_eq!(failed.state, TaskState::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("fixed protocol failure")
    );
    let ledger = rt.model_requests(&id).unwrap();
    assert_eq!(ledger.settled_tokens, 8);
    assert_eq!(ledger.reserved_tokens, 0);
    assert!(!ledger.budget_unknown);
    assert_eq!(
        ledger.requests[0].terminal,
        Some(ModelRequestTerminal::Failed)
    );
    assert_eq!(ledger.requests[0].settled_tokens, Some(8));
    assert_eq!(
        ledger.requests[0]
            .observations
            .last()
            .unwrap()
            .observation
            .event,
        "error"
    );
    unknown_money(&ledger);
    drop(rt);
    let reopened = open(root.path(), provider.clone());
    assert_eq!(reopened.model_requests(&id).unwrap(), ledger);
    assert_eq!(reopened.task(&failed.id).unwrap().state, TaskState::Failed);
    prompt(&reopened, &id).await;
    let after = reopened.model_requests(&id).unwrap();
    assert_eq!(after.settled_tokens, 16);
    assert_eq!(after.reserved_tokens, 0);
    assert!(!after.budget_unknown);
    assert_eq!(after.requests.len(), 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(reopened.task(&failed.id).unwrap().state, TaskState::Failed);
    assert!(
        reopened
            .tasks(&id)
            .unwrap()
            .iter()
            .any(|task| task.state == TaskState::Completed)
    );
}

#[tokio::test]
async fn error_with_missing_partial_or_invalid_usage_retains_its_hold_after_reopen() {
    for (status, invalid_total) in [
        (UsageStatus::Unknown, false),
        (UsageStatus::Partial, false),
        (UsageStatus::Invalid, false),
        (UsageStatus::Reported, true),
    ] {
        let root = root();
        let provider = Arc::new(FailedThenDone {
            calls: AtomicUsize::new(0),
            status,
            invalid_total,
        });
        let rt = open(root.path(), provider.clone());
        let id = rt.create(None).unwrap().id;
        prompt(&rt, &id).await;
        assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Failed);
        let ledger = rt.model_requests(&id).unwrap();
        assert_eq!(ledger.settled_tokens, 0);
        assert_eq!(ledger.reserved_tokens, 1000);
        assert!(ledger.budget_unknown);
        drop(rt);
        let reopened = open(root.path(), provider.clone());
        assert_eq!(reopened.model_requests(&id).unwrap(), ledger);
        prompt(&reopened, &id).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(reopened.model_requests(&id).unwrap().reserved_tokens, 1000);
    }
}

fn unknown_money(ledger: &ModelRequestLedger) {
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
}
async fn prompt(rt: &Runtime, session: &str) {
    tokio::time::timeout(
        Duration::from_secs(30),
        rt.prompt(session, "fixed boundary input", CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
}
fn seed_identity(lease: &AttemptLease, id: String) -> Value {
    json!({"request_id":id,"lease":lease,"task_id":"synthetic-stored-model-task",
        "budget_session_id":lease.session_id,"turn":1,"provider":"fixture","model":"model-ledger-boundary"})
}
// Append valid synthetic Host records in one fixture transaction. The real Core
// creates/ends the lease; no production row is deleted or existing event changed.
fn seed<I: IntoIterator<Item = Value>>(
    rt: &Runtime,
    session: &str,
    entries: impl FnOnce(&AttemptLease, u64) -> I,
) {
    let lease = rt
        .store()
        .begin_attempt(session, "synthetic-boundary-owner")
        .unwrap();
    let snapshot = rt.store().snapshot(session).unwrap();
    let records = entries(&lease, snapshot.cursor + 1);
    let database = rt.workspace().parent().unwrap().join("session.db");
    let mut db = Connection::open(database).unwrap();
    let tx = db.transaction().unwrap();
    let mut cursor = snapshot.cursor;
    {
        let mut insert = tx
            .prepare("INSERT INTO core_events(session_id,cursor,record) VALUES(?1,?2,?3)")
            .unwrap();
        for entry in records {
            cursor += 1;
            let event = json!({"protocol_version":3,"session_id":session,"cursor":cursor,"timestamp":1,
                "attempt":lease,"event":{"type":"host","kind":"model_request","data":entry}});
            insert
                .execute(params![
                    session,
                    i64::try_from(cursor).unwrap(),
                    event.to_string()
                ])
                .unwrap();
        }
    }
    tx.execute(
        "UPDATE core_sessions SET projection=json_set(projection,'$.cursor',?2) WHERE id=?1",
        params![session, i64::try_from(cursor).unwrap()],
    )
    .unwrap();
    tx.commit().unwrap();
    rt.store().finish_attempt(&lease).unwrap();
}
fn rejections(rt: &Runtime, session: &str, count: usize) {
    seed(rt, session, |lease, _| {
        (0..count).map(|index| json!({
        "record":"rejected","identity":seed_identity(lease, format!("synthetic-rejected-{index}")),
        "token_limit":1000,"reason":"synthetic stored rejection for capacity boundary"
    })).collect::<Vec<_>>()
    });
}
#[tokio::test]
async fn unrelated_roots_do_not_share_request_record_capacity() {
    let root = root();
    let provider = Arc::new(Fixed::default());
    let rt = open(root.path(), provider.clone());
    let full = rt.create(None).unwrap().id;
    rejections(&rt, &full, 4096);
    let unrelated = rt.create(None).unwrap().id;
    rejections(&rt, &unrelated, 1);
    let other = rt
        .model_requests(&unrelated)
        .expect("an unrelated root must remain readable");
    assert_eq!(other.requests.len(), 1);
    assert_eq!(other.remaining_tokens, 1000);
    assert_eq!(rt.model_requests(&full).unwrap().requests.len(), 4096);
    prompt(&rt, &unrelated).await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert_eq!(rt.model_requests(&unrelated).unwrap().settled_tokens, 8);
}

#[tokio::test]
async fn full_request_history_rejects_before_reserve_without_poisoning_existing_records() {
    let root = root();
    let provider = Arc::new(Fixed::default());
    let rt = open(root.path(), provider.clone());
    let id = rt.create(None).unwrap().id;
    rejections(&rt, &id, 4096);
    let before = rt.model_requests(&id).unwrap();
    prompt(&rt, &id).await;
    assert_eq!(
        provider.0.load(Ordering::SeqCst),
        0,
        "full history must reject before Provider::stream"
    );
    assert_eq!(rt.model_requests(&id).unwrap(), before);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Failed);
    drop(rt);
    let reopened = open(root.path(), provider);
    assert_eq!(reopened.model_requests(&id).unwrap(), before);
}

#[tokio::test]
async fn zero_output_model_is_rejected_before_storage_or_provider_invocation() {
    let root = root();
    let provider = Arc::new(Fixed::default());
    let rt = open(root.path(), provider.clone());
    let id = rt.create(None).unwrap().id;
    let before = rt.model_requests(&id).unwrap();
    let mut zero = model();
    zero.max_output_tokens = Some(0);
    let candidate = Runtime::open(
        root.path().join("session.db"),
        root.path().join("workspace"),
        provider.clone(),
        zero,
        config(),
    );
    assert!(
        matches!(candidate, Err(RuntimeError::InvalidInput(_))),
        "zero output must fail SDK admission"
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    assert_eq!(rt.model_requests(&id).unwrap(), before);
    prompt(&rt, &id).await;
    assert_eq!(rt.model_requests(&id).unwrap().settled_tokens, 8);
}

struct CancelBeforeDone {
    peer: Mutex<Option<(Runtime, String)>>,
    calls: AtomicUsize,
}
impl Provider for CancelBeforeDone {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (peer, task) = self.peer.lock().unwrap().take().unwrap();
        // Commit the exact peer cancellation in the same poll as this ready Done.
        // The owner token is still false; no timer or lifecycle polling orders it.
        assert!(!cancel.is_cancelled());
        let target = peer.task(&task).unwrap().cancel_target();
        assert!(peer.cancel(&target).unwrap());
        assert!(peer.task(&task).unwrap().cancel_requested);
        assert!(!cancel.is_cancelled());
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: answer(&request, true),
        }]))
    }
}
#[tokio::test]
async fn committed_peer_cancel_prevents_late_reported_done_from_settling() {
    let root = root();
    let provider = Arc::new(CancelBeforeDone {
        peer: Mutex::new(None),
        calls: AtomicUsize::new(0),
    });
    let owner = open(root.path(), provider.clone());
    let peer = open(root.path(), provider.clone());
    let id = owner.create(None).unwrap().id;
    let task = owner.enqueue(&id, "cancel before ready Done").unwrap();
    *provider.peer.lock().unwrap() = Some((peer.clone(), task.id.clone()));
    owner
        .resume(&task.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(owner.task(&task.id).unwrap().state, TaskState::Cancelled);
    let ledger = owner.model_requests(&id).unwrap();
    assert_eq!(
        ledger.settled_tokens, 0,
        "late Done must not release a cancelled reservation"
    );
    assert_eq!(ledger.reserved_tokens, 1000);
    assert_eq!(
        ledger.requests[0].terminal,
        Some(ModelRequestTerminal::Cancelled)
    );
    assert!(
        ledger.requests[0]
            .observations
            .iter()
            .any(
                |entry| entry.observation.usage_status == UsageStatus::Reported
                    && entry.observation.usage.total_tokens == 8
            )
    );
    unknown_money(&ledger);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(owner);
    drop(peer);
    assert_eq!(
        open(root.path(), provider).model_requests(&id).unwrap(),
        ledger
    );
}

struct PausedUnknown {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    calls: AtomicUsize,
}
impl Provider for PausedUnknown {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        let message = answer(&request, false);
        Box::pin(async_stream::stream! {
            entered.add_permits(1);
            release.acquire_owned().await.unwrap().forget();
            yield AssistantEvent::Done { message };
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_branches_compete_for_one_hold_and_unknown_survives_new_fork_and_reopen() {
    let root = root();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let provider = Arc::new(PausedUnknown {
        entered: entered.clone(),
        release: release.clone(),
        calls: AtomicUsize::new(0),
    });
    let owner = open(root.path(), provider.clone());
    let peer = open(root.path(), provider.clone());
    let id = owner.create(None).unwrap().id;
    let branch = owner.fork(&id, "before unknown", true).unwrap().id;
    let run = {
        let owner = owner.clone();
        let branch = branch.clone();
        tokio::spawn(async move { prompt(&owner, &branch).await })
    };
    tokio::time::timeout(Duration::from_secs(30), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    prompt(&peer, &id).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(peer.tasks(&id).unwrap()[0].state, TaskState::Failed);
    release.add_permits(1);
    run.await.unwrap();
    let after = owner.fork(&branch, "after unknown", true).unwrap().id;
    let ledger = owner.model_requests(&after).unwrap();
    assert_eq!(ledger.budget_session_id, id);
    assert_eq!(ledger.reserved_tokens, 1000);
    assert_eq!(ledger.settled_tokens, 0);
    assert!(ledger.budget_unknown);
    assert_eq!(owner.model_requests(&branch).unwrap(), ledger);
    assert_eq!(peer.model_requests(&id).unwrap(), ledger);
    drop(owner);
    drop(peer);
    let reopened = open(root.path(), provider.clone());
    assert_eq!(reopened.model_requests(&after).unwrap(), ledger);
    prompt(&reopened, &after).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let final_ledger = reopened.model_requests(&id).unwrap();
    assert_eq!(final_ledger.reserved_tokens, 1000);
    unknown_money(&final_ledger);
}

#[tokio::test]
async fn persistent_observed_and_finished_write_failures_keep_only_the_original_durable_hold() {
    let root = root();
    let provider = Arc::new(Fixed::default());
    let rt = open(root.path(), provider.clone());
    let id = rt.create(None).unwrap().id;
    let db = Connection::open(root.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER persistent_model_record_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='model_request' AND json_extract(NEW.record,'$.event.data.record') IN ('observed','finished') BEGIN SELECT RAISE(ABORT,'persistent fixture failure'); END;").unwrap();
    assert!(
        rt.prompt(
            &id,
            "known local response with unavailable recording",
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_ne!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let ledger = rt.model_requests(&id).unwrap();
    assert_eq!(ledger.reserved_tokens, 1000);
    assert_eq!(ledger.settled_tokens, 0);
    assert!(ledger.budget_unknown);
    assert!(ledger.requests[0].observations.is_empty());
    assert_eq!(
        ledger.requests[0].terminal, None,
        "failed writes must not invent a durable terminal"
    );
    unknown_money(&ledger);
    let records = rt.events(&id, 0, 1000).unwrap();
    let kinds = records
        .into_iter()
        .filter_map(|entry| match entry.event {
            CoreEvent::Host { kind, data } if kind == "model_request" => {
                Some(data["record"].as_str().unwrap().to_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["reserved", "started"]);
    drop(db);
    drop(rt);
    assert_eq!(
        open(root.path(), provider.clone())
            .model_requests(&id)
            .unwrap(),
        ledger
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
}
