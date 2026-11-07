//! Deterministic public-SDK fixture regressions for durable model budget continuation grants.
//!
//! When an earlier model request in a session's lineage ends in an unknown hold
//! (e.g. timeout, upstream crash, or unverified usage), the entire budget tree enters
//! `budget_unknown = true`. A trusted Rust-host-only durable continuation grant allows
//! a newly queued task on the same session to proceed with finite headroom, while
//! retaining the old historical hold intact.
//!
//! Offline synthetic providers exercise the public SDK. Some ledger boundary
//! cases seed historical model records through a local SQL fixture. The tool
//! unknown regression uses the normal Runtime, CodeMode and Host execution path.
//! No live models or network calls are used.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_codemode::CapabilityError;
use cayw_core::{
    CapabilityOutcome, CapabilityReconciliation, ReconciliationRequest, ReconciliationVerifier,
    SessionState,
};
use cayw_host::ExternalHost;
use cayw_runtime::{
    ModelBudgetContinuation, ModelRequestOrigin, ModelRequestReconciledOutcome,
    ModelRequestReconciliation, ModelRequestReconciliationVerifier, ModelRequestTerminal, Runtime,
    RuntimeConfig, RuntimeError, ScopedPermission, TaskState,
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Semaphore;

fn model() -> Model {
    let mut model = Model::new("fixture", "model-budget-continuation");
    model.max_output_tokens = Some(64);
    model
}

fn config(max_tokens: u64) -> RuntimeConfig {
    RuntimeConfig {
        max_total_tokens: max_tokens,
        reconciliation_verifier: Some(Arc::new(AllowAllVerifier)),
        model_request_reconciliation_verifier: Some(Arc::new(AllowAllVerifier)),
        ..Default::default()
    }
}

fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    root
}

fn open(root: &Path, provider: Arc<dyn Provider>, max_tokens: u64) -> Runtime {
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        provider,
        model(),
        config(max_tokens),
    )
    .unwrap()
}

fn open_with_config(root: &Path, provider: Arc<dyn Provider>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        provider,
        model(),
        config,
    )
    .unwrap()
}

struct AllowAllVerifier;
impl ModelRequestReconciliationVerifier for AllowAllVerifier {
    fn verify(&self, _: &ModelRequestReconciliation) -> Result<(), String> {
        Ok(())
    }
}
impl ReconciliationVerifier for AllowAllVerifier {
    fn verify(&self, _: &ReconciliationRequest) -> Result<(), String> {
        Ok(())
    }
}

fn unreconciled_holds(rt: &Runtime, session_id: &str) -> Vec<ModelRequestOrigin> {
    rt.model_requests(session_id)
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.outstanding_reservation_tokens > 0)
        .map(ModelRequestOrigin::from)
        .collect()
}

enum ScriptStep {
    /// Return a terminal assistant message with reported token usage.
    Success {
        input_tokens: u64,
        output_tokens: u64,
        content: String,
    },
    /// Return a tool call to `execute_code` with the given script to trigger another turn.
    ToolCall {
        input_tokens: u64,
        output_tokens: u64,
        call_id: String,
        script: String,
    },
    /// Return an error with Unknown usage status (producing an unknown reservation hold).
    UnknownError,
    /// Pause at a semaphore before emitting the next step (for deterministic concurrency testing).
    Paused {
        entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
        inner: Box<ScriptStep>,
    },
}

struct ScriptedProvider {
    steps: Mutex<VecDeque<ScriptStep>>,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            steps: Mutex::new(VecDeque::from(steps)),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ScriptStep::Success {
                input_tokens: 4,
                output_tokens: 4,
                content: "fallback answer".into(),
            });

        match step {
            ScriptStep::Success {
                input_tokens,
                output_tokens,
                content,
            } => {
                let mut message = AssistantMessage::new(&request.model);
                message.content.push(Content::text(content));
                message.stop_reason = StopReason::Stop;
                message.usage = Usage {
                    input: input_tokens,
                    output: output_tokens,
                    total_tokens: input_tokens + output_tokens,
                    ..Default::default()
                };
                message.usage_status = UsageStatus::Reported;
                Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
            }
            ScriptStep::ToolCall {
                input_tokens,
                output_tokens,
                call_id,
                script,
            } => {
                let mut message = AssistantMessage::new(&request.model);
                message.stop_reason = StopReason::ToolUse;
                message.usage = Usage {
                    input: input_tokens,
                    output: output_tokens,
                    total_tokens: input_tokens + output_tokens,
                    ..Default::default()
                };
                message.usage_status = UsageStatus::Reported;
                message.content.push(Content::ToolCall(ToolCall::new(
                    call_id,
                    "execute_code",
                    json!({
                        "action": "exec",
                        "script": script,
                        "yield_time_ms": 30_000,
                        "max_output_tokens": 1024,
                    }),
                )));
                Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
            }
            ScriptStep::UnknownError => {
                let mut message = AssistantMessage::new(&request.model);
                message.usage_status = UsageStatus::Unknown;
                message.stop_reason = StopReason::Error;
                Box::pin(futures::stream::iter([AssistantEvent::Error {
                    message: message.failure(StopReason::Error, "synthetic unknown failure"),
                }]))
            }
            ScriptStep::Paused {
                entered,
                release,
                inner,
            } => {
                let model = request.model.clone();
                Box::pin(async_stream::stream! {
                    entered.add_permits(1);
                    let _ = release.acquire().await;
                    if cancel.is_cancelled() {
                        let mut message = AssistantMessage::new(&model);
                        message.stop_reason = StopReason::Aborted;
                        yield AssistantEvent::Error {
                            message: message.failure(StopReason::Aborted, "cancelled"),
                        };
                        return;
                    }
                    match *inner {
                        ScriptStep::Success { input_tokens, output_tokens, content } => {
                            let mut message = AssistantMessage::new(&model);
                            message.content.push(Content::text(content));
                            message.stop_reason = StopReason::Stop;
                            message.usage = Usage {
                                input: input_tokens,
                                output: output_tokens,
                                total_tokens: input_tokens + output_tokens,
                                ..Default::default()
                            };
                            message.usage_status = UsageStatus::Reported;
                            yield AssistantEvent::Done { message };
                        }
                        ScriptStep::UnknownError => {
                            let mut message = AssistantMessage::new(&model);
                            message.usage_status = UsageStatus::Unknown;
                            message.stop_reason = StopReason::Error;
                            yield AssistantEvent::Error {
                                message: message.failure(StopReason::Error, "synthetic unknown failure"),
                            };
                        }
                        _ => {
                            let mut message = AssistantMessage::new(&model);
                            message.stop_reason = StopReason::Stop;
                            yield AssistantEvent::Done { message };
                        }
                    }
                })
            }
        }
    }
}

/// Seed an exact historical model reservation hold into a session database
/// using standard host events, mirroring production sessions that retained
/// an unknown hold from an earlier interrupted run.
fn seed_unknown_hold(
    rt: &Runtime,
    session: &str,
    task_id: &str,
    request_id: &str,
    reserved_tokens: u64,
    token_limit: u64,
) -> ModelRequestOrigin {
    let lease = rt
        .store()
        .begin_attempt(session, "synthetic-historical-worker")
        .unwrap();
    let snapshot = rt.store().snapshot(session).unwrap();
    let database = rt.workspace().parent().unwrap().join("session.db");
    let mut db = Connection::open(database).unwrap();
    let tx = db.transaction().unwrap();
    let mut cursor = snapshot.cursor;

    let identity = json!({
        "request_id": request_id,
        "lease": lease,
        "task_id": task_id,
        "budget_session_id": session,
        "turn": 1,
        "provider": "fixture",
        "model": "model-budget-continuation"
    });

    let entries = [
        json!({
            "record": "reserved",
            "identity": identity,
            "policy": "entire_session_remainder",
            "tokens": reserved_tokens,
            "token_limit": token_limit,
            "output_max_tokens": 64
        }),
        json!({
            "record": "started",
            "request_id": request_id,
            "reservation_cursor": cursor + 1,
            "previous_cursor": cursor + 1
        }),
        json!({
            "record": "finished",
            "request_id": request_id,
            "reservation_cursor": cursor + 1,
            "previous_cursor": cursor + 2,
            "terminal": "failed",
            "settled_tokens": null,
            "last_received": null
        }),
    ];

    {
        let mut insert = tx
            .prepare("INSERT INTO core_events(session_id,cursor,record) VALUES(?1,?2,?3)")
            .unwrap();
        for entry in entries {
            cursor += 1;
            let event = json!({
                "protocol_version": 3,
                "session_id": session,
                "cursor": cursor,
                "timestamp": 1,
                "attempt": lease,
                "event": {
                    "type": "host",
                    "kind": "model_request",
                    "data": entry
                }
            });
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

    let ledger = rt.model_requests(session).unwrap();
    ModelRequestOrigin::from(&ledger.requests[0])
}

// ---------------------------------------------------------------------------
// 1. Failure with unknown hold -> new queued same-session grant with finite
//    headroom -> multi-turn successful reported calls leave old origin/hold
//    unchanged; subsequent ungranted task remains rejected.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_continuation_grant_allows_new_queued_task_and_preserves_old_hold() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turns for Task 2 (granted continuation):
        // Turn 1: tool call (consumes 10 tokens)
        ScriptStep::ToolCall {
            input_tokens: 6,
            output_tokens: 4,
            call_id: "tool-call-1".into(),
            script: "return 42".into(),
        },
        // Turn 2: final answer (consumes 15 tokens)
        ScriptStep::Success {
            input_tokens: 10,
            output_tokens: 5,
            content: "continuation completed successfully".into(),
        },
    ]));

    let rt = open(root.path(), provider.clone(), 2000);
    let session = rt.create(Some("budget continuation test".into())).unwrap();
    let session_id = session.id;

    // Seed historical unknown hold: 1000 tokens within 2000 tree limit
    let old_origin = seed_unknown_hold(
        &rt,
        &session_id,
        "historical-task-0",
        "historical-request-0",
        1000,
        2000,
    );

    let ledger = rt.model_requests(&session_id).unwrap();
    assert!(ledger.budget_unknown);
    assert_eq!(ledger.token_limit, 2000);
    assert_eq!(ledger.reserved_tokens, 1000);
    assert_eq!(ledger.remaining_tokens, 1000);
    assert_eq!(ledger.settled_tokens, 0);
    assert_eq!(ledger.requests.len(), 1);
    assert_eq!(
        ledger.requests[0].terminal,
        Some(ModelRequestTerminal::Failed)
    );
    assert_eq!(ledger.requests[0].settled_tokens, None);
    assert_eq!(ledger.requests[0].outstanding_reservation_tokens, 1000);

    // An ungranted Task 1 is queued; when resumed, it must fail admission immediately
    let task1 = rt.enqueue(&session_id, "ungranted task 1").unwrap();
    let res = rt.resume(&task1.id, CancellationToken::new()).await;
    assert!(res.is_err() || rt.task(&task1.id).unwrap().state == TaskState::Failed);
    assert_eq!(rt.task(&task1.id).unwrap().state, TaskState::Failed);

    // Queue Task 2: will receive an explicit continuation grant
    let task2 = rt
        .enqueue(&session_id, "granted continuation task")
        .unwrap();

    // Authorize continuation grant for Task 2
    let grant = ModelBudgetContinuation {
        task_id: task2.id.clone(),
        expected: vec![old_origin.clone()],
        additional_tokens: 200,
        token_limit: 1500,
        actor: "trusted-host".into(),
        evidence: "audit-continuation-grant-01".into(),
    };
    let grant_record = rt
        .authorize_model_budget_continuation(grant.clone())
        .await
        .unwrap();

    assert_eq!(grant_record.session_id, session_id);
    assert_eq!(grant_record.continuation.task_id, task2.id);
    assert_eq!(grant_record.continuation.additional_tokens, 200);
    assert_eq!(grant_record.continuation.token_limit, 1500);
    assert!(grant_record.cursor > 0);

    // Verify ledger records the continuation and preserves old hold intact
    let ledger_before_run = rt.model_requests(&session_id).unwrap();
    assert_eq!(ledger_before_run.continuations.len(), 1);
    assert_eq!(ledger_before_run.continuations[0], grant_record);
    assert!(ledger_before_run.budget_unknown);
    assert_eq!(ledger_before_run.requests[0].reservation_tokens, 1000);
    assert_eq!(
        ledger_before_run.requests[0].outstanding_reservation_tokens,
        1000
    );
    assert_eq!(ledger_before_run.requests[0].settled_tokens, None);

    // Run Task 2: executes multi-turn successfully
    let resume_res = rt.resume(&task2.id, CancellationToken::new()).await;
    assert!(
        resume_res.is_ok(),
        "granted task should succeed: {resume_res:?}"
    );
    assert_eq!(rt.task(&task2.id).unwrap().state, TaskState::Completed);

    // Verify final accounting
    let final_ledger = rt.model_requests(&session_id).unwrap();
    assert!(final_ledger.budget_unknown); // Old unknown hold is still retained
    assert_eq!(final_ledger.continuations.len(), 1);

    // Old request (index 0) remains completely unchanged
    assert_eq!(final_ledger.requests[0].reservation_tokens, 1000);
    assert_eq!(
        final_ledger.requests[0].outstanding_reservation_tokens,
        1000
    );
    assert_eq!(final_ledger.requests[0].settled_tokens, None);
    assert_eq!(
        ModelRequestOrigin::from(&final_ledger.requests[0]),
        old_origin
    );

    // Task 2 settled requests: turn 1 = 10 tokens, turn 2 = 15 tokens, total = 25 tokens
    let task2_settled: u64 = final_ledger
        .requests
        .iter()
        .filter(|r| r.identity.task_id == task2.id)
        .map(|r| r.settled_tokens.unwrap_or(0))
        .sum();
    assert_eq!(task2_settled, 25);
    assert_eq!(final_ledger.settled_tokens, 25);

    // Subsequent Task 3 without a grant MUST be rejected because old unknown hold remains
    let task3 = rt
        .enqueue(&session_id, "subsequent ungranted task")
        .unwrap();
    let res3 = rt.resume(&task3.id, CancellationToken::new()).await;
    assert!(res3.is_err() || rt.task(&task3.id).unwrap().state == TaskState::Failed);
    assert_eq!(rt.task(&task3.id).unwrap().state, TaskState::Failed);
}

// ---------------------------------------------------------------------------
// 2. Rejection of implicit allowance from config increase alone, reopen,
//    fork, or ungranted task.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_no_implicit_allowance_from_config_increase_reopen_fork_or_ungranted_task() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptStep::UnknownError,
        ScriptStep::Success {
            input_tokens: 4,
            output_tokens: 4,
            content: "should not be reachable".into(),
        },
    ]));

    let rt = open(root.path(), provider.clone(), 1000);
    let session = rt.create(None).unwrap();
    let id = session.id;

    // Initial failure produces unknown hold
    let task1 = rt.enqueue(&id, "fail").unwrap();
    let _ = rt.resume(&task1.id, CancellationToken::new()).await;
    assert_eq!(rt.task(&task1.id).unwrap().state, TaskState::Failed);

    let ledger = rt.model_requests(&id).unwrap();
    assert!(ledger.budget_unknown);

    // (a) Reopen without config change: ungranted task fails
    drop(rt);
    let reopened = open(root.path(), provider.clone(), 1000);
    assert!(reopened.model_requests(&id).unwrap().budget_unknown);
    let ungranted_reopened = reopened.enqueue(&id, "ungranted after reopen").unwrap();
    let _ = reopened
        .resume(&ungranted_reopened.id, CancellationToken::new())
        .await;
    assert_eq!(
        reopened.task(&ungranted_reopened.id).unwrap().state,
        TaskState::Failed
    );

    // (b) Config increase: reopen with max_total_tokens = 10,000. Still budget_unknown!
    drop(reopened);
    let upgraded = open(root.path(), provider.clone(), 10_000);
    let upgraded_ledger = upgraded.model_requests(&id).unwrap();
    assert!(upgraded_ledger.budget_unknown);
    let ungranted_upgraded = upgraded
        .enqueue(&id, "ungranted after config increase")
        .unwrap();
    let _ = upgraded
        .resume(&ungranted_upgraded.id, CancellationToken::new())
        .await;
    assert_eq!(
        upgraded.task(&ungranted_upgraded.id).unwrap().state,
        TaskState::Failed
    );

    // (c) Fork: forking inherits the budget lineage root; ungranted task on fork fails
    let fork_view = upgraded.fork(&id, "fork branch", true).unwrap();
    let fork_id = fork_view.id;
    let fork_ledger = upgraded.model_requests(&fork_id).unwrap();
    assert_eq!(fork_ledger.budget_session_id, id);
    assert!(fork_ledger.budget_unknown);

    let ungranted_fork = upgraded.enqueue(&fork_id, "ungranted on fork").unwrap();
    let _ = upgraded
        .resume(&ungranted_fork.id, CancellationToken::new())
        .await;
    assert_eq!(
        upgraded.task(&ungranted_fork.id).unwrap().state,
        TaskState::Failed
    );
}

// ---------------------------------------------------------------------------
// 3. Validation: wrong/stale/partial expected, wrong task reused/running,
//    missing headroom, zero/overflow amount, duplicate contradictory grants
//    rejected; exact same grant is idempotent.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_continuation_grant_validation_and_idempotence() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let rt = open(root.path(), provider.clone(), 2000);
    let session = rt.create(None).unwrap();
    let id = session.id;

    // Seed valid unknown hold: 1000 tokens within 2000 token limit
    let valid_origin = seed_unknown_hold(
        &rt,
        &id,
        "historical-task-validation",
        "historical-req-validation",
        1000,
        2000,
    );

    let new_task = rt.enqueue(&id, "target task").unwrap();

    // 1. Empty expected rejected
    let bad_grant_empty = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_empty)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 2. Wrong request_id in expected rejected
    let mut bad_origin = valid_origin.clone();
    bad_origin.identity.request_id = "nonexistent-request-id".into();
    let bad_grant_origin = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![bad_origin],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_origin)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 3. Stale cursor / wrong reservation tokens in expected rejected
    let mut bad_origin_tokens = valid_origin.clone();
    bad_origin_tokens.reservation_tokens = 999_999;
    let bad_grant_tokens = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![bad_origin_tokens],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_tokens)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 4. Duplicate expected origins rejected
    let bad_grant_duplicate_expected = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone(), valid_origin.clone()],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_duplicate_expected)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 5. Target task does not exist rejected
    let bad_grant_task = ModelBudgetContinuation {
        task_id: "nonexistent-task-id".into(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_task).await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 6. Old task id (historical request task id) reused rejected
    let bad_grant_reused = ModelBudgetContinuation {
        task_id: valid_origin.identity.task_id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_reused)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 7. additional_tokens == 0 rejected
    let bad_grant_zero = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 0,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_zero).await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 8. token_limit > trusted current tree token limit rejected
    // tree limit is 2000; grant specifies token_limit: 2500
    let bad_grant_tree_limit = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 100,
        token_limit: 2500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_tree_limit)
            .await,
        Err(RuntimeError::Budget(_))
    ));

    // 9. checked(occupied + additional) > token_limit rejected (missing headroom)
    // occupied is 1000, additional is 600, token_limit is 1500 (1000 + 600 = 1600 > 1500)
    let bad_grant_headroom = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 600,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_headroom)
            .await,
        Err(RuntimeError::Budget(_))
    ));

    // 10. Arithmetic overflow on additional_tokens rejected
    let bad_grant_overflow = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: u64::MAX,
        token_limit: 2000,
        actor: "host".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_overflow)
            .await,
        Err(RuntimeError::Budget(_))
    ));

    // 11. Empty actor or evidence rejected
    let bad_grant_actor = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "".into(),
        evidence: "evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_grant_actor)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // 12. Idempotence: authorize valid grant
    let valid_grant = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 200,
        token_limit: 1500,
        actor: "trusted-host".into(),
        evidence: "valid-evidence".into(),
    };

    let rec1 = rt
        .authorize_model_budget_continuation(valid_grant.clone())
        .await
        .unwrap();

    // Re-submitting the EXACT same grant returns the existing record without appending
    let rec2 = rt
        .authorize_model_budget_continuation(valid_grant.clone())
        .await
        .unwrap();
    assert_eq!(rec1, rec2);

    let ledger_after = rt.model_requests(&id).unwrap();
    assert_eq!(ledger_after.continuations.len(), 1);

    // 13. Submitting a contradictory grant for the same task is rejected
    let contradictory_grant = ModelBudgetContinuation {
        task_id: new_task.id.clone(),
        expected: vec![valid_origin.clone()],
        additional_tokens: 300, // different amount!
        token_limit: 1500,
        actor: "trusted-host".into(),
        evidence: "valid-evidence".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(contradictory_grant)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
}

// ---------------------------------------------------------------------------
// 4. Verification that a new unknown request blocks further requests despite
//    a prior grant or config increase.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_new_unknown_blocks_further_requests_despite_prior_grant() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Task 2: granted, but Turn 1 produces another unknown error
        ScriptStep::UnknownError,
        // Task 3: should never be reached
        ScriptStep::Success {
            input_tokens: 4,
            output_tokens: 4,
            content: "unreachable".into(),
        },
    ]));

    let rt = open(root.path(), provider.clone(), 2000);
    let session = rt.create(None).unwrap();
    let id = session.id;

    // Seed historical unknown hold: 1000 tokens within 2000 tree limit
    let origin1 = seed_unknown_hold(
        &rt,
        &id,
        "historical-task-1",
        "historical-req-1",
        1000,
        2000,
    );

    // Task 2 receives grant for origin1
    let task2 = rt.enqueue(&id, "task 2").unwrap();
    let grant = ModelBudgetContinuation {
        task_id: task2.id.clone(),
        expected: vec![origin1.clone()],
        additional_tokens: 200,
        token_limit: 1500,
        actor: "host".into(),
        evidence: "grant-for-task2".into(),
    };
    rt.authorize_model_budget_continuation(grant).await.unwrap();

    // Run Task 2: it encounters a new unknown error!
    let _ = rt.resume(&task2.id, CancellationToken::new()).await;
    assert_eq!(rt.task(&task2.id).unwrap().state, TaskState::Failed);

    let ledger = rt.model_requests(&id).unwrap();
    assert!(ledger.budget_unknown);
    // Now there are 2 requests in the ledger, both unknown
    assert_eq!(ledger.requests.len(), 2);
    assert_eq!(
        ledger.requests[1].terminal,
        Some(ModelRequestTerminal::Failed)
    );
    assert_eq!(ledger.requests[1].outstanding_reservation_tokens, 200);

    // Queue Task 3: even if someone tries to grant with only origin1, it must be rejected
    // because EVERY outstanding unknown in the entire lineage must be in expected
    let task3 = rt.enqueue(&id, "task 3").unwrap();
    let bad_partial_grant = ModelBudgetContinuation {
        task_id: task3.id.clone(),
        expected: vec![origin1.clone()], // misses origin2!
        additional_tokens: 100,
        token_limit: 1800,
        actor: "host".into(),
        evidence: "stale-grant".into(),
    };
    assert!(matches!(
        rt.authorize_model_budget_continuation(bad_partial_grant)
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));

    // And attempting to resume Task 3 without a valid complete grant fails
    let res3 = rt.resume(&task3.id, CancellationToken::new()).await;
    assert!(res3.is_err() || rt.task(&task3.id).unwrap().state == TaskState::Failed);
    assert_eq!(rt.task(&task3.id).unwrap().state, TaskState::Failed);
}

// ---------------------------------------------------------------------------
// 5. Reopen preserves durable continuation grant and enforces finite cap.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_reopen_preserves_grant_finite_cap() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turn 1 of granted task (consumes 30 tokens)
        ScriptStep::ToolCall {
            input_tokens: 20,
            output_tokens: 10,
            call_id: "turn1-call".into(),
            script: "return 1".into(),
        },
        // Turn 2 of granted task after reopen (consumes 40 tokens)
        ScriptStep::Success {
            input_tokens: 25,
            output_tokens: 15,
            content: "reopened turn 2 completed".into(),
        },
    ]));

    let rt = open(root.path(), provider.clone(), 2000);
    let session = rt.create(None).unwrap();
    let id = session.id;

    // Seed historical unknown hold: 1000 tokens within 2000 tree limit
    let origin = seed_unknown_hold(
        &rt,
        &id,
        "historical-task-cap",
        "historical-req-cap",
        1000,
        2000,
    );

    let task2 = rt.enqueue(&id, "task 2").unwrap();
    let grant = ModelBudgetContinuation {
        task_id: task2.id.clone(),
        expected: vec![origin.clone()],
        additional_tokens: 100,
        token_limit: 1500,
        actor: "trusted-host".into(),
        evidence: "reopen-cap-evidence".into(),
    };
    let rec = rt.authorize_model_budget_continuation(grant).await.unwrap();

    // Close and reopen the runtime before executing task2
    drop(rt);
    let reopened = open(root.path(), provider.clone(), 2000);

    let ledger = reopened.model_requests(&id).unwrap();
    assert_eq!(ledger.continuations.len(), 1);
    assert_eq!(ledger.continuations[0], rec);

    // Run Task 2 on the reopened runtime
    let res = reopened.resume(&task2.id, CancellationToken::new()).await;
    assert!(
        res.is_ok(),
        "task2 should execute under durable grant: {res:?}"
    );
    assert_eq!(
        reopened.task(&task2.id).unwrap().state,
        TaskState::Completed
    );

    let final_ledger = reopened.model_requests(&id).unwrap();
    assert_eq!(final_ledger.settled_tokens, 70); // 30 + 40
    assert_eq!(final_ledger.continuations.len(), 1);
    assert_eq!(final_ledger.requests[0].reservation_tokens, 1000);
}

// ---------------------------------------------------------------------------
// 6. Old later reconciliation frees old hold but DOES NOT inflate granted
//    task allowance:
//    Limit EVERY request of a granted task, even when expected old hold later
//    reconciles, to MIN(tree available, grant.token_limit - total occupied,
//    grant.additional_tokens - all settled/reserved consumption of target task).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_later_reconciliation_frees_old_hold_without_inflating_granted_task_allowance() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Initial failure producing 1000 tokens hold
        ScriptStep::UnknownError,
        // Turn 1 of granted task: tool call consuming 50 tokens
        ScriptStep::ToolCall {
            input_tokens: 30,
            output_tokens: 20,
            call_id: "turn1-call".into(),
            script: "return 'turn 1 done'".into(),
        },
        // Turn 2 of granted task: completes, reporting 60 tokens
        ScriptStep::Success {
            input_tokens: 40,
            output_tokens: 20,
            content: "turn 2 done".into(),
        },
    ]));

    let mut cfg = config(1000);
    cfg.model_request_reconciliation_verifier = Some(Arc::new(AllowAllVerifier));
    let rt = open_with_config(root.path(), provider.clone(), cfg);

    let session = rt.create(None).unwrap();
    let id = session.id;

    let task1 = rt.enqueue(&id, "fail").unwrap();
    let _ = rt.resume(&task1.id, CancellationToken::new()).await;
    assert_eq!(rt.task(&task1.id).unwrap().state, TaskState::Failed);

    let ledger = rt.model_requests(&id).unwrap();
    let old_origin = ModelRequestOrigin::from(&ledger.requests[0]);
    assert_eq!(old_origin.reservation_tokens, 1000);
    assert!(ledger.budget_unknown);

    drop(rt);
    let mut cfg = config(1200);
    cfg.model_request_reconciliation_verifier = Some(Arc::new(AllowAllVerifier));
    let rt = open_with_config(root.path(), provider.clone(), cfg);

    let task2 = rt.enqueue(&id, "granted task 2").unwrap();
    let grant = ModelBudgetContinuation {
        task_id: task2.id.clone(),
        expected: vec![old_origin.clone()],
        additional_tokens: 150,
        token_limit: 1200,
        actor: "trusted-host".into(),
        evidence: "finite-cap-reconciliation-proof".into(),
    };
    rt.authorize_model_budget_continuation(grant).await.unwrap();

    // Reconcile the old hold before running task 2
    let reconciliation = ModelRequestReconciliation {
        expected: old_origin.clone(),
        actor: "reconcile-actor".into(),
        evidence: "proved-not-dispatched".into(),
        outcome: ModelRequestReconciledOutcome::NotDispatched {
            reason: "synthetic upstream drop confirmed".into(),
        },
    };
    let rec_res = rt.reconcile_model_request(reconciliation).await.unwrap();
    assert_eq!(rec_res.reconciliation.expected, old_origin);

    // Verify old hold is now settled to 0 tokens and budget_unknown is false!
    let ledger_after_reconcile = rt.model_requests(&id).unwrap();
    assert!(!ledger_after_reconcile.budget_unknown);
    assert_eq!(ledger_after_reconcile.requests[0].settled_tokens, Some(0));
    assert_eq!(
        ledger_after_reconcile.requests[0].outstanding_reservation_tokens,
        0
    );

    // Now tree available is 1200 tokens!
    // But Task 2 was granted ONLY 150 tokens.
    // Task 2's request admission must STILL be capped to:
    // MIN(tree available, grant.token_limit - total occupied, grant.additional_tokens - task consumption)
    // The release of the 1000-token hold does NOT expand Task 2's remaining allowance!
    let resume_res = rt.resume(&task2.id, CancellationToken::new()).await;
    assert!(resume_res.is_ok(), "task2 should finish: {resume_res:?}");
    assert_eq!(rt.task(&task2.id).unwrap().state, TaskState::Completed);

    let final_ledger = rt.model_requests(&id).unwrap();
    let task2_requests: Vec<_> = final_ledger
        .requests
        .iter()
        .filter(|r| r.identity.task_id == task2.id)
        .collect();

    assert_eq!(task2_requests.len(), 2);
    // Turn 1 reservation was clamped to at most 150 tokens
    assert!(task2_requests[0].reservation_tokens <= 150);
    // Turn 2 reservation was clamped to at most (150 - 50) = 100 tokens, NOT 1200 or 1000!
    assert!(
        task2_requests[1].reservation_tokens <= 100,
        "Turn 2 reservation ({}) must not exceed remaining grant allowance (100) despite freed tree hold",
        task2_requests[1].reservation_tokens
    );
}

// ---------------------------------------------------------------------------
// 7. Busy/lease fence safety remains enforced across the budget lineage.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_busy_fence_remains_enforced() {
    let root = root();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));

    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turn 1 on root session: returns an unknown error to create a real ledger origin
        ScriptStep::UnknownError,
        // Task 1 on root session: Pauses inside provider to keep attempt active
        ScriptStep::Paused {
            entered: entered.clone(),
            release: release.clone(),
            inner: Box::new(ScriptStep::UnknownError),
        },
    ]));

    let rt = open(root.path(), provider.clone(), 1000);
    let session = rt.create(None).unwrap();
    let root_id = session.id;

    // Run initial task on root to generate a genuine unknown hold with a real ledger origin
    let init_task = rt.enqueue(&root_id, "initial unknown hold").unwrap();
    let _ = rt.resume(&init_task.id, CancellationToken::new()).await;
    let initial_origins = unreconciled_holds(&rt, &root_id);
    assert_eq!(initial_origins.len(), 1);
    let real_origin = initial_origins[0].clone();

    // Reopen with config increase to 3000 tokens so continuation grant has headroom
    drop(rt);
    let rt = open(root.path(), provider.clone(), 3000);

    // Fork a child session before starting Task 1 so both share the same budget lineage
    let fork_view = rt.fork(&root_id, "lineage branch", true).unwrap();
    let fork_id = fork_view.id;

    // Enqueue Task 1 on root session and authorize a continuation grant so it is admitted
    let task1 = rt.enqueue(&root_id, "running task on root").unwrap();
    let task1_grant = ModelBudgetContinuation {
        task_id: task1.id.clone(),
        expected: vec![real_origin.clone()],
        additional_tokens: 200,
        token_limit: 2000,
        actor: "host".into(),
        evidence: "task1-grant".into(),
    };
    rt.authorize_model_budget_continuation(task1_grant)
        .await
        .unwrap();

    // Enqueue Task 2 on the fork branch
    let task2 = rt.enqueue(&fork_id, "queued task on fork").unwrap();

    // Start Task 1 on root session in background
    let rt_clone = rt.clone();
    let task1_id = task1.id.clone();
    let handle =
        tokio::spawn(async move { rt_clone.resume(&task1_id, CancellationToken::new()).await });

    // Wait until provider is entered (task 1 is actively running on root session)
    entered.acquire().await.unwrap().forget();
    assert_eq!(rt.task(&task1.id).unwrap().state, TaskState::Running);

    // Continuation grant targeting task 2 using the real ledger origin from root session
    let grant = ModelBudgetContinuation {
        task_id: task2.id.clone(),
        expected: vec![real_origin.clone()],
        additional_tokens: 100,
        token_limit: 2500,
        actor: "host".into(),
        evidence: "fence-test".into(),
    };

    // Calling authorize while lineage has an active attempt MUST fail with Busy!
    let busy_res = rt.authorize_model_budget_continuation(grant).await;
    assert!(
        matches!(busy_res, Err(RuntimeError::Busy)),
        "authorizing continuation while lineage is busy must return Busy, got: {busy_res:?}"
    );

    // Release task 1 so it finishes its unknown error
    release.add_permits(1);
    let _ = handle.await.unwrap();
    assert_eq!(rt.task(&task1.id).unwrap().state, TaskState::Failed);

    // Now all sessions in the lineage are idle; busy check passes.
    // Invalid expected input (empty expected) is rejected with InvalidInput, not Busy.
    let idle_res = rt
        .authorize_model_budget_continuation(ModelBudgetContinuation {
            task_id: task2.id.clone(),
            expected: vec![],
            additional_tokens: 100,
            token_limit: 1500,
            actor: "host".into(),
            evidence: "fence-test".into(),
        })
        .await;
    assert!(
        matches!(idle_res, Err(RuntimeError::InvalidInput(_))),
        "idle session should reject invalid input, not Busy"
    );

    // With all outstanding holds (both init_task and task1), continuation authorization succeeds
    let idle_holds = unreconciled_holds(&rt, &root_id);
    assert_eq!(idle_holds.len(), 2);
    let valid_grant = rt
        .authorize_model_budget_continuation(ModelBudgetContinuation {
            task_id: task2.id.clone(),
            expected: idle_holds,
            additional_tokens: 100,
            token_limit: 1500,
            actor: "host".into(),
            evidence: "fence-test".into(),
        })
        .await;
    assert!(
        valid_grant.is_ok(),
        "idle session should accept continuation grant with real ledger origins: {valid_grant:?}"
    );
}

struct UnknownSearch {
    calls: AtomicUsize,
}

#[async_trait]
impl ExternalHost for UnknownSearch {
    async fn call(
        &self,
        capability: &str,
        arguments: serde_json::Value,
        _: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        assert_eq!(capability, "search.query");
        assert_eq!(arguments, json!({"query": "fixture"}));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CapabilityError::unknown("fixture service lost its result"))
    }
}

#[tokio::test]
async fn test_granted_task_tool_unknown_blocks_until_original_capability_reconciled() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptStep::UnknownError,
        ScriptStep::ToolCall {
            input_tokens: 6,
            output_tokens: 4,
            call_id: "unknown-search-call".into(),
            script: "return cayw.call('search.query', {query = 'fixture'})".into(),
        },
        ScriptStep::Success {
            input_tokens: 10,
            output_tokens: 5,
            content: "completed after original capability reconciliation".into(),
        },
    ]));
    let external = Arc::new(UnknownSearch {
        calls: AtomicUsize::new(0),
    });
    let mut initial_config = config(1000);
    initial_config.host.external = Some(external.clone());
    initial_config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query": "fixture"}),
    )];
    let rt = open_with_config(root.path(), provider.clone(), initial_config.clone());
    let id = rt.create(None).unwrap().id;
    let original_task = rt.enqueue(&id, "produce historical model hold").unwrap();
    let _ = rt.resume(&original_task.id, CancellationToken::new()).await;
    assert_eq!(rt.task(&original_task.id).unwrap().state, TaskState::Failed);
    let old_request = rt.model_requests(&id).unwrap().requests[0].clone();
    assert_eq!(old_request.outstanding_reservation_tokens, 1000);
    drop(rt);

    initial_config.max_total_tokens = 3000;
    let rt = open_with_config(root.path(), provider.clone(), initial_config);
    let tool_task = rt
        .enqueue(&id, "granted task encounters tool unknown")
        .unwrap();
    let grant = ModelBudgetContinuation {
        task_id: tool_task.id.clone(),
        expected: vec![ModelRequestOrigin::from(&old_request)],
        additional_tokens: 200,
        token_limit: 1500,
        actor: "fixture-host".into(),
        evidence: "finite grant before the tool outcome becomes unknown".into(),
    };
    let granted = rt
        .authorize_model_budget_continuation(grant.clone())
        .await
        .unwrap();
    let _ = rt.resume(&tool_task.id, CancellationToken::new()).await;
    let unknown = rt.get(&id).unwrap();
    assert_eq!(unknown.state, SessionState::NeedsReconciliation);
    assert!(unknown.attempt.is_none());
    assert_eq!(
        rt.task(&tool_task.id).unwrap().state,
        TaskState::OutcomeUnknown
    );
    assert_eq!(unknown.unresolved_capabilities.len(), 1);
    let call = unknown.unresolved_capabilities[0].clone();
    assert_eq!(call.task_id, tool_task.id);
    assert_eq!(call.lease.session_id, id);
    assert_eq!(call.name, "search.query");
    assert_eq!(call.parent_call_id, "unknown-search-call");
    assert!(matches!(
        call.outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert_eq!(external.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    // Re-reading an existing grant cannot clear the independent tool unknown.
    assert_eq!(
        rt.authorize_model_budget_continuation(grant).await.unwrap(),
        granted
    );
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(matches!(
        rt.enqueue(&id, "must remain blocked"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert!(
        rt.resume(&tool_task.id, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(external.calls.load(Ordering::SeqCst), 1);

    let reconciliation = CapabilityReconciliation {
        key: call.key.clone(),
        expected: (&call).into(),
        actor: "fixture-reconciler".into(),
        evidence: "fixture original-effect receipt".into(),
        outcome: CapabilityOutcome::Completed {
            result: json!({"hits": []}),
        },
        external_provenance: call.external_provenance.clone(),
    };
    let mut mismatched = reconciliation.clone();
    mismatched.expected.task_id = "wrong-task-id".into();
    assert!(matches!(
        rt.reconcile_capability(&id, mismatched),
        Err(RuntimeError::InvalidInput(_))
    ));
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    let reconciled = rt.reconcile_capability(&id, reconciliation).unwrap();
    assert_eq!(reconciled.state, SessionState::Idle);
    assert!(reconciled.unresolved_capabilities.is_empty());
    assert!(reconciled.unresolved_calls.is_empty());

    let next = rt
        .enqueue(&id, "complete original work after reconciliation")
        .unwrap();
    rt.authorize_model_budget_continuation(ModelBudgetContinuation {
        task_id: next.id.clone(),
        expected: unreconciled_holds(&rt, &id),
        additional_tokens: 200,
        token_limit: 1500,
        actor: "fixture-host".into(),
        evidence: "explicit finite grant for the new task after reconciliation".into(),
    })
    .await
    .unwrap();
    let completed = rt.resume(&next.id, CancellationToken::new()).await.unwrap();
    assert_eq!(completed.state, SessionState::Idle);
    assert_eq!(rt.task(&next.id).unwrap().state, TaskState::Completed);
    assert!(!rt.task(&next.id).unwrap().holds_slot);
    let final_ledger = rt.model_requests(&id).unwrap();
    assert_eq!(
        final_ledger
            .requests
            .iter()
            .find(|r| r.identity.request_id == old_request.identity.request_id),
        Some(&old_request)
    );
    assert_eq!(final_ledger.reserved_tokens, 1000);
    assert_eq!(final_ledger.settled_tokens, 25);
    assert!(final_ledger.budget_unknown);
    assert_eq!(final_ledger.continuations.len(), 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        external.calls.load(Ordering::SeqCst),
        1,
        "the unknown capability must not be replayed"
    );
}

// ---------------------------------------------------------------------------
// 8. Fork continuation with root cursor lag completes within finite cap.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_fork_continuation_with_root_cursor_lag() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turn 1 on root session: unknown error creates historical hold
        ScriptStep::UnknownError,
        // Turn 2 for fork task after replay: finishes successfully within finite cap
        ScriptStep::Success {
            input_tokens: 30,
            output_tokens: 20,
            content: "fork task completed successfully".into(),
        },
    ]));

    let rt = open(root.path(), provider.clone(), 2000);
    let session = rt.create(None).unwrap();
    let root_id = session.id;

    // Fork a child session BEFORE any model requests exist in root
    let fork_view = rt.fork(&root_id, "child fork", true).unwrap();
    let fork_id = fork_view.id;
    let fork_control_cursor = fork_view.cursor;

    // Now run a task on root session that ends in an unknown hold
    let root_task = rt.enqueue(&root_id, "task creating root hold").unwrap();
    let _ = rt.resume(&root_task.id, CancellationToken::new()).await;
    assert_eq!(rt.task(&root_task.id).unwrap().state, TaskState::Failed);

    let root_holds = unreconciled_holds(&rt, &root_id);
    assert_eq!(root_holds.len(), 1);
    let root_hold = root_holds[0].clone();

    // Verify root hold cursor is strictly greater than the fork control cursor
    let hold_cursor = root_hold.reservation_cursor.unwrap();
    assert!(
        hold_cursor > fork_control_cursor,
        "historical hold reservation cursor ({hold_cursor}) must be > fork control cursor ({fork_control_cursor})"
    );

    // Verify fork session's budget tree is in budget_unknown state due to root ancestor hold
    let fork_ledger = rt.model_requests(&fork_id).unwrap();
    assert!(
        fork_ledger.budget_unknown,
        "fork ledger must reflect budget_unknown due to root hold"
    );

    // Enqueue an ungranted task on fork to verify unknown hold blocks admission
    let ungranted_fork_task = rt.enqueue(&fork_id, "ungranted fork task").unwrap();
    let unauth_res = rt
        .resume(&ungranted_fork_task.id, CancellationToken::new())
        .await;
    assert!(
        unauth_res.is_err() || rt.task(&ungranted_fork_task.id).unwrap().state == TaskState::Failed
    );
    assert_eq!(
        rt.task(&ungranted_fork_task.id).unwrap().state,
        TaskState::Failed
    );

    // Reopen with config increase to 3000 tokens so continuation grant has headroom
    drop(rt);
    let rt = open(root.path(), provider.clone(), 3000);

    // Enqueue newly queued target task on fork
    let fork_task = rt
        .enqueue(&fork_id, "task on fork with cursor lag")
        .unwrap();

    // Authorize model budget continuation on fork explicitly naming the root hold
    let grant = ModelBudgetContinuation {
        task_id: fork_task.id.clone(),
        expected: vec![root_hold.clone()],
        additional_tokens: 200,
        token_limit: 2500,
        actor: "host-regression".into(),
        evidence: "fork-cursor-lag-evidence".into(),
    };
    let grant_record = rt.authorize_model_budget_continuation(grant).await.unwrap();
    assert_eq!(grant_record.continuation.task_id, fork_task.id);

    // Simulate replay / drop and reopen from disk in the same process
    drop(rt);
    let rt_reopened = open(root.path(), provider.clone(), 3000);

    // Resume the fork task in the reopened runtime instance
    let complete_view = rt_reopened
        .resume(&fork_task.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(complete_view.state, SessionState::Idle);

    let finished_task = rt_reopened.task(&fork_task.id).unwrap();
    assert_eq!(finished_task.state, TaskState::Completed);

    // Verify execution actually completed within the finite continuation cap
    let reopened_ledger = rt_reopened.model_requests(&fork_id).unwrap();
    assert_eq!(reopened_ledger.continuations.len(), 1);
    assert_eq!(
        reopened_ledger.continuations[0].continuation.task_id,
        fork_task.id
    );

    let task_requests: Vec<_> = reopened_ledger
        .requests
        .iter()
        .filter(|r| r.identity.task_id == fork_task.id)
        .collect();
    assert_eq!(task_requests.len(), 1);
    let settled = task_requests[0].settled_tokens.unwrap();
    assert_eq!(settled, 50); // 30 input + 20 output
    assert!(
        settled <= 200,
        "task settled tokens ({settled}) must be within finite cap (200)"
    );
}

// ---------------------------------------------------------------------------
// 9. Rejection of cancelled and already-dispatched target tasks.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_continuation_rejects_cancelled_or_dispatched_tasks() {
    let root = root();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turn 1 on root session: unknown error creates real historical hold
        ScriptStep::UnknownError,
    ]));

    let rt = open(root.path(), provider.clone(), 2000);
    let session = rt.create(None).unwrap();
    let root_id = session.id;

    // Run initial task to create real unknown hold in ledger
    let init_task = rt.enqueue(&root_id, "init task").unwrap();
    let _ = rt.resume(&init_task.id, CancellationToken::new()).await;
    assert_eq!(rt.task(&init_task.id).unwrap().state, TaskState::Failed);

    let holds = unreconciled_holds(&rt, &root_id);
    assert_eq!(holds.len(), 1);
    let hold = holds[0].clone();

    // Case 1: Target task is cancelled while queued
    let task_to_cancel = rt.enqueue(&root_id, "task to cancel").unwrap();
    let cancelled = rt.cancel(&task_to_cancel.cancel_target()).unwrap();
    assert!(cancelled);
    assert_eq!(
        rt.task(&task_to_cancel.id).unwrap().state,
        TaskState::Cancelled
    );

    let cancel_res = rt
        .authorize_model_budget_continuation(ModelBudgetContinuation {
            task_id: task_to_cancel.id.clone(),
            expected: vec![hold.clone()],
            additional_tokens: 100,
            token_limit: 1500,
            actor: "host".into(),
            evidence: "cancelled-task-test".into(),
        })
        .await;
    assert!(
        matches!(cancel_res, Err(RuntimeError::InvalidInput(_))),
        "authorizing continuation for cancelled task must be rejected with InvalidInput, got: {cancel_res:?}"
    );

    // Verify no grant was allocated for the cancelled task
    let ledger_after_cancel = rt.model_requests(&root_id).unwrap();
    assert!(
        ledger_after_cancel
            .continuations
            .iter()
            .all(|c| c.continuation.task_id != task_to_cancel.id),
        "no continuation grant must be allocated for cancelled task"
    );

    // Case 2: Target task is already dispatched (init_task ran and is terminal, dispatches > 0)
    let dispatched_task = rt.task(&init_task.id).unwrap();
    assert!(dispatched_task.dispatches > 0);

    let dispatch_res = rt
        .authorize_model_budget_continuation(ModelBudgetContinuation {
            task_id: init_task.id.clone(),
            expected: vec![hold.clone()],
            additional_tokens: 100,
            token_limit: 1500,
            actor: "host".into(),
            evidence: "dispatched-task-test".into(),
        })
        .await;
    assert!(
        matches!(dispatch_res, Err(RuntimeError::InvalidInput(_))),
        "authorizing continuation for already-dispatched task must be rejected with InvalidInput, got: {dispatch_res:?}"
    );

    // Verify no grant was allocated for the already-dispatched task
    let ledger_after_dispatch = rt.model_requests(&root_id).unwrap();
    assert!(
        ledger_after_dispatch
            .continuations
            .iter()
            .all(|c| c.continuation.task_id != init_task.id),
        "no continuation grant must be allocated for already-dispatched task"
    );
}
