//! Verification tests for Responses provider usage details in observation and reconciliation.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_runtime::{
    ModelRequestOrigin, ModelRequestReconciledOutcome, ModelRequestReconciliation,
    ModelRequestReconciliationVerifier, ModelUsageObservation, Runtime, RuntimeConfig,
    RuntimeError,
};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::timeout;

const BOUND: Duration = Duration::from_secs(20);
const LIMIT: u64 = 1000;

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

struct StreamingUsageProvider {
    events: Vec<AssistantEvent>,
    calls: AtomicUsize,
}
impl Provider for StreamingUsageProvider {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

fn open(
    root: &Path,
    provider: Arc<dyn Provider>,
    verifier: Option<Arc<dyn ModelRequestReconciliationVerifier>>,
    limit: u64,
) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut model = Model::new("responses", "gpt-4o-realtime");
    model.max_output_tokens = Some(16);
    Runtime::open(
        root.join("sessions.db"),
        workspace,
        provider,
        model,
        RuntimeConfig {
            max_total_tokens: limit,
            model_request_reconciliation_verifier: verifier,
            ..Default::default()
        },
    )
    .unwrap()
}

fn base_message(model: &Model) -> AssistantMessage {
    let mut message = AssistantMessage::new(model);
    message
        .content
        .push(Content::text("partial responses output"));
    message.response_id = Some("resp-test-01".into());
    message.response_model = Some("gpt-4o-realtime".into());
    message
}

#[tokio::test]
async fn responses_usage_update_details_admitted_without_redaction_and_reconciles() {
    let temp = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let model = Model::new("responses", "gpt-4o-realtime");

    let mut update_msg = base_message(&model);
    update_msg.usage_status = UsageStatus::Reported;
    update_msg.usage = Usage {
        input: 80,
        output: 6,
        cache_read: 3,
        cache_write: 2,
        reasoning: 4,
        total_tokens: 91,
    };
    update_msg.usage_fields = serde_json::from_value(json!({
        "input_tokens": 85,
        "output_tokens": 6,
        "total_tokens": 91,
        "input_tokens_details": {
            "cached_tokens": 3,
            "cache_write_tokens": 2
        },
        "output_tokens_details": {
            "reasoning_tokens": 4
        }
    }))
    .unwrap();

    let err_msg = base_message(&model).failure(
        StopReason::Error,
        "interrupted streaming before complete finish",
    );

    let provider = Arc::new(StreamingUsageProvider {
        events: vec![
            AssistantEvent::TextDelta {
                index: 0,
                delta: "partial responses output".into(),
                partial: update_msg.clone(),
            },
            AssistantEvent::UsageUpdate {
                partial: update_msg.clone(),
            },
            AssistantEvent::Error { message: err_msg },
        ],
        calls: AtomicUsize::new(0),
    });

    let runtime = open(temp.path(), provider.clone(), Some(receipts.clone()), LIMIT);
    let session = runtime.create(None).unwrap().id;

    timeout(
        BOUND,
        runtime.prompt(&session, "test responses details", CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    runtime.wait_idle(&session).await;

    let ledger = runtime.model_requests(&session).unwrap();
    assert_eq!(ledger.requests.len(), 1);
    let record = &ledger.requests[0];
    assert_eq!(record.settled_tokens, None);
    assert_eq!(record.outstanding_reservation_tokens, LIMIT);
    assert!(ledger.budget_unknown);

    // Verify observation chain retained details without redaction
    assert!(!record.observations.is_empty());
    let update_obs = record
        .observations
        .iter()
        .find(|entry| entry.observation.event == "usage_update")
        .expect("must contain usage_update observation");
    assert!(!update_obs.observation.metadata_redacted);
    assert_eq!(
        update_obs
            .observation
            .usage_fields
            .get("input_tokens_details"),
        Some(&json!({"cached_tokens": 3, "cache_write_tokens": 2}))
    );
    assert_eq!(
        update_obs
            .observation
            .usage_fields
            .get("output_tokens_details"),
        Some(&json!({"reasoning_tokens": 4}))
    );

    // Now construct trusted reconciliation report
    let full_report = ModelUsageObservation {
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
            "input_tokens": 85,
            "output_tokens": 12,
            "total_tokens": 97,
            "input_tokens_details": {
                "cached_tokens": 3,
                "cache_write_tokens": 2
            },
            "output_tokens_details": {
                "reasoning_tokens": 7
            }
        }))
        .unwrap(),
        response_id: Some("resp-test-01".into()),
        response_model: Some("gpt-4o-realtime".into()),
        stop_reason: StopReason::Stop,
        metadata_redacted: false,
    };

    let decision = ModelRequestReconciliation {
        expected: ModelRequestOrigin::from(record),
        actor: "trusted-verifier".into(),
        evidence: format!(
            "complete responses capture for {}",
            record.identity.request_id
        ),
        outcome: ModelRequestReconciledOutcome::ReportedUsage {
            report: full_report,
        },
    };

    receipts.accept(&decision);
    runtime
        .reconcile_model_request(decision.clone())
        .await
        .unwrap();

    let updated_ledger = runtime.model_requests(&session).unwrap();
    let updated_record = &updated_ledger.requests[0];
    assert_eq!(updated_record.settled_tokens, Some(97));
    assert_eq!(updated_record.outstanding_reservation_tokens, 0);
    assert_eq!(updated_ledger.settled_tokens, 97);
    assert_eq!(updated_ledger.reserved_tokens, 0);
    assert_eq!(updated_ledger.remaining_tokens, LIMIT.saturating_sub(97));
    assert!(!updated_ledger.budget_unknown);

    // Reopen and verify persistence
    drop(runtime);
    let reopened = open(temp.path(), provider.clone(), Some(receipts.clone()), LIMIT);
    let reopened_ledger = reopened.model_requests(&session).unwrap();
    let reopened_record = &reopened_ledger.requests[0];
    assert_eq!(reopened_record.settled_tokens, Some(97));
    assert_eq!(reopened_record.outstanding_reservation_tokens, 0);
    assert!(!reopened_ledger.budget_unknown);
}

#[tokio::test]
async fn responses_unknown_detail_fields_are_redacted_and_reconciliation_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let model = Model::new("responses", "gpt-4o-realtime");

    let mut update_msg = base_message(&model);
    update_msg.usage_status = UsageStatus::Reported;
    update_msg.usage = Usage {
        input: 80,
        output: 6,
        cache_read: 3,
        cache_write: 2,
        total_tokens: 91,
        ..Default::default()
    };
    // Include unwhitelisted key inside input_tokens_details
    update_msg.usage_fields = serde_json::from_value(json!({
        "input_tokens": 85,
        "output_tokens": 6,
        "total_tokens": 91,
        "input_tokens_details": {
            "cached_tokens": 3,
            "cache_write_tokens": 2,
            "unwhitelisted_secret": 999
        }
    }))
    .unwrap();

    let err_msg = base_message(&model).failure(StopReason::Error, "interrupted streaming");

    let provider = Arc::new(StreamingUsageProvider {
        events: vec![
            AssistantEvent::UsageUpdate {
                partial: update_msg.clone(),
            },
            AssistantEvent::Error { message: err_msg },
        ],
        calls: AtomicUsize::new(0),
    });

    let runtime = open(temp.path(), provider.clone(), Some(receipts.clone()), LIMIT);
    let session = runtime.create(None).unwrap().id;

    runtime
        .prompt(
            &session,
            "test unwhitelisted details",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    runtime.wait_idle(&session).await;

    let ledger = runtime.model_requests(&session).unwrap();
    let record = &ledger.requests[0];
    assert_eq!(record.settled_tokens, None);
    assert_eq!(record.outstanding_reservation_tokens, LIMIT);

    // Verify metadata_redacted was set to true on the observation
    let update_obs = record
        .observations
        .iter()
        .find(|entry| entry.observation.event == "usage_update")
        .expect("must contain usage_update observation");
    assert!(update_obs.observation.metadata_redacted);

    // Attempting reconciliation must be rejected because prior observation was redacted
    let full_report = ModelUsageObservation {
        event: "done".into(),
        usage_status: UsageStatus::Reported,
        usage: Usage {
            input: 80,
            output: 12,
            cache_read: 3,
            cache_write: 2,
            total_tokens: 97,
            ..Default::default()
        },
        usage_fields: serde_json::from_value(json!({
            "input_tokens": 85,
            "output_tokens": 12,
            "total_tokens": 97,
            "input_tokens_details": {
                "cached_tokens": 3,
                "cache_write_tokens": 2
            }
        }))
        .unwrap(),
        response_id: Some("resp-test-01".into()),
        response_model: Some("gpt-4o-realtime".into()),
        stop_reason: StopReason::Stop,
        metadata_redacted: false,
    };

    let decision = ModelRequestReconciliation {
        expected: ModelRequestOrigin::from(record),
        actor: "trusted-verifier".into(),
        evidence: format!("reconcile attempt for {}", record.identity.request_id),
        outcome: ModelRequestReconciledOutcome::ReportedUsage {
            report: full_report,
        },
    };

    receipts.accept(&decision);
    let err = runtime.reconcile_model_request(decision).await.unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidInput(_)));

    // Budget hold is preserved
    let final_ledger = runtime.model_requests(&session).unwrap();
    assert_eq!(final_ledger.requests[0].settled_tokens, None);
    assert_eq!(
        final_ledger.requests[0].outstanding_reservation_tokens,
        LIMIT
    );
    assert!(final_ledger.budget_unknown);
}

#[tokio::test]
async fn responses_non_integer_detail_triggers_redaction_and_rejects_reconcile() {
    let temp = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let model = Model::new("responses", "gpt-4o-realtime");

    let mut update_msg = base_message(&model);
    update_msg.usage_status = UsageStatus::Reported;
    update_msg.usage = Usage {
        input: 80,
        output: 6,
        cache_read: 3,
        cache_write: 2,
        total_tokens: 91,
        ..Default::default()
    };
    // Non-numeric value for cached_tokens
    update_msg.usage_fields = serde_json::from_value(json!({
        "input_tokens": 85,
        "output_tokens": 6,
        "total_tokens": 91,
        "input_tokens_details": {
            "cached_tokens": "string_not_number",
            "cache_write_tokens": 2
        }
    }))
    .unwrap();

    let err_msg = base_message(&model).failure(StopReason::Error, "interrupted streaming");

    let provider = Arc::new(StreamingUsageProvider {
        events: vec![
            AssistantEvent::UsageUpdate {
                partial: update_msg.clone(),
            },
            AssistantEvent::Error { message: err_msg },
        ],
        calls: AtomicUsize::new(0),
    });

    let runtime = open(temp.path(), provider.clone(), Some(receipts.clone()), LIMIT);
    let session = runtime.create(None).unwrap().id;

    runtime
        .prompt(
            &session,
            "test non-numeric detail",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    runtime.wait_idle(&session).await;

    let ledger = runtime.model_requests(&session).unwrap();
    let record = &ledger.requests[0];
    let update_obs = record
        .observations
        .iter()
        .find(|entry| entry.observation.event == "usage_update")
        .expect("must contain usage_update observation");
    assert!(update_obs.observation.metadata_redacted);

    let full_report = ModelUsageObservation {
        event: "done".into(),
        usage_status: UsageStatus::Reported,
        usage: Usage {
            input: 80,
            output: 12,
            cache_read: 3,
            cache_write: 2,
            total_tokens: 97,
            ..Default::default()
        },
        usage_fields: serde_json::from_value(json!({
            "input_tokens": 85,
            "output_tokens": 12,
            "total_tokens": 97,
            "input_tokens_details": {
                "cached_tokens": 3,
                "cache_write_tokens": 2
            }
        }))
        .unwrap(),
        response_id: Some("resp-test-01".into()),
        response_model: Some("gpt-4o-realtime".into()),
        stop_reason: StopReason::Stop,
        metadata_redacted: false,
    };

    let decision = ModelRequestReconciliation {
        expected: ModelRequestOrigin::from(record),
        actor: "trusted-verifier".into(),
        evidence: "reconcile attempt".into(),
        outcome: ModelRequestReconciledOutcome::ReportedUsage {
            report: full_report,
        },
    };

    receipts.accept(&decision);
    let err = runtime.reconcile_model_request(decision).await.unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidInput(_)));
}

#[tokio::test]
async fn responses_decreasing_detail_counter_in_report_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::default());
    let model = Model::new("responses", "gpt-4o-realtime");

    let mut update_msg = base_message(&model);
    update_msg.usage_status = UsageStatus::Reported;
    update_msg.usage = Usage {
        input: 80,
        output: 6,
        cache_read: 5,
        cache_write: 2,
        total_tokens: 93,
        ..Default::default()
    };
    // Observation has cached_tokens = 5
    update_msg.usage_fields = serde_json::from_value(json!({
        "input_tokens": 87,
        "output_tokens": 6,
        "total_tokens": 93,
        "input_tokens_details": {
            "cached_tokens": 5,
            "cache_write_tokens": 2
        }
    }))
    .unwrap();

    let err_msg = base_message(&model).failure(StopReason::Error, "interrupted streaming");

    let provider = Arc::new(StreamingUsageProvider {
        events: vec![
            AssistantEvent::UsageUpdate {
                partial: update_msg.clone(),
            },
            AssistantEvent::Error { message: err_msg },
        ],
        calls: AtomicUsize::new(0),
    });

    let runtime = open(temp.path(), provider.clone(), Some(receipts.clone()), LIMIT);
    let session = runtime.create(None).unwrap().id;

    runtime
        .prompt(
            &session,
            "test non-monotonic detail",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    runtime.wait_idle(&session).await;

    let ledger = runtime.model_requests(&session).unwrap();
    let record = &ledger.requests[0];

    // Report specifies cached_tokens = 3 (< 5 in observation), which violates monotonic prefix
    let decreasing_report = ModelUsageObservation {
        event: "done".into(),
        usage_status: UsageStatus::Reported,
        usage: Usage {
            input: 82,
            output: 12,
            cache_read: 3,
            cache_write: 2,
            total_tokens: 99,
            ..Default::default()
        },
        usage_fields: serde_json::from_value(json!({
            "input_tokens": 87,
            "output_tokens": 12,
            "total_tokens": 99,
            "input_tokens_details": {
                "cached_tokens": 3,
                "cache_write_tokens": 2
            }
        }))
        .unwrap(),
        response_id: Some("resp-test-01".into()),
        response_model: Some("gpt-4o-realtime".into()),
        stop_reason: StopReason::Stop,
        metadata_redacted: false,
    };

    let decision = ModelRequestReconciliation {
        expected: ModelRequestOrigin::from(record),
        actor: "trusted-verifier".into(),
        evidence: "reconcile attempt with decreasing cached_tokens".into(),
        outcome: ModelRequestReconciledOutcome::ReportedUsage {
            report: decreasing_report,
        },
    };

    receipts.accept(&decision);
    let err = runtime.reconcile_model_request(decision).await.unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidInput(_)));
}
