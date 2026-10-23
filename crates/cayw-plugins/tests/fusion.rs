#![cfg(feature = "fusion")]
use cayw_ai::Usage;
use cayw_plugins::{fusion::*, *};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Candidate {
    id: String,
    response: CandidateResponse,
    seen: Arc<Mutex<Vec<(String, FusionInput)>>>,
    hang: bool,
}
#[async_trait]
impl FusionCandidateSource for Candidate {
    fn provider_id(&self) -> &str {
        &self.id
    }
    async fn answer(
        &self,
        input: Arc<FusionInput>,
        _cancel: CancellationToken,
    ) -> CandidateResponse {
        self.seen
            .lock()
            .unwrap()
            .push((self.id.clone(), (*input).clone()));
        if self.hang {
            futures::future::pending().await
        } else {
            self.response.clone()
        }
    }
}
fn input() -> FusionInput {
    FusionInput {
        question: "offline answer".into(),
        canonical_context: json!({"b":2,"a":1}),
        constraints: vec!["do not use network".into()],
        answer_schema: json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"],"additionalProperties":false}),
    }
}
fn candidates(
    values: Vec<CandidateResponse>,
    seen: &Arc<Mutex<Vec<(String, FusionInput)>>>,
) -> Vec<Arc<dyn FusionCandidateSource>> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, response)| {
            Arc::new(Candidate {
                id: format!("provider-{index}"),
                response,
                seen: seen.clone(),
                hang: false,
            }) as Arc<dyn FusionCandidateSource>
        })
        .collect()
}
fn answer(n: i32) -> CandidateResponse {
    CandidateResponse::Answer {
        value: json!({"n":n}),
        usage: Some(Usage {
            input: 2,
            output: 3,
            total_tokens: 5,
            ..Default::default()
        }),
    }
}
#[tokio::test]
async fn fusion_identical_inputs_preserve_answers_failures_usage_and_disagreements() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let report = collect_fusion(
        input(),
        candidates(
            vec![
                answer(1),
                answer(2),
                CandidateResponse::Failed {
                    message: "offline fixture failure".into(),
                    usage: Some(Usage {
                        input: 1,
                        total_tokens: 1,
                        ..Default::default()
                    }),
                },
            ],
            &seen,
        ),
        Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|(_, value)| value == &input())
    );
    assert!(
        report
            .candidates
            .iter()
            .all(|candidate| candidate.input_digest == report.input_digest)
    );
    assert_eq!(
        report.disagreements,
        vec![("provider-0".into(), "provider-1".into())]
    );
    assert!(
        matches!(&report.candidates[0].outcome, CandidateOutcome::Answer { value, usage: Some(usage) } if value == &json!({"n":1}) && usage.total_tokens == 5)
    );
    assert!(
        matches!(&report.candidates[2].outcome, CandidateOutcome::Failed { usage: Some(usage), .. } if usage.total_tokens == 1)
    );
}
#[tokio::test]
async fn fusion_strict_schema_rejects_coercion_and_keeps_invalid_answer() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sources = candidates(
        vec![
            answer(1),
            CandidateResponse::Answer {
                value: json!({"n":"1"}),
                usage: None,
            },
            answer(1),
        ],
        &seen,
    );
    let report = collect_fusion(
        input(),
        sources,
        Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(&report.candidates[1].outcome, CandidateOutcome::InvalidAnswer { value, .. } if value == &json!({"n":"1"}))
    );
    assert!(report.disagreements.is_empty());
}
#[tokio::test]
async fn fusion_enforces_three_to_sixteen_distinct_providers_before_any_calls() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    for count in [0, 2, 17] {
        assert!(
            collect_fusion(
                input(),
                candidates(vec![answer(1); count], &seen),
                Duration::from_secs(1),
                CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    let candidate: Arc<dyn FusionCandidateSource> = Arc::new(Candidate {
        id: "duplicate".into(),
        response: answer(1),
        seen: seen.clone(),
        hang: false,
    });
    assert!(
        collect_fusion(
            input(),
            vec![candidate.clone(), candidate.clone(), candidate],
            Duration::from_secs(1),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn fusion_timeouts_and_cancellation_keep_every_candidate_terminal() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut sources = candidates(vec![answer(1), answer(1)], &seen);
    sources.push(Arc::new(Candidate {
        id: "hanging".into(),
        response: answer(0),
        seen: seen.clone(),
        hang: true,
    }));
    let report = collect_fusion(
        input(),
        sources,
        Duration::from_millis(100),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(
        report.candidates[2].outcome,
        CandidateOutcome::TimedOut
    ));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let report = collect_fusion(
        input(),
        candidates(vec![answer(1); 3], &seen),
        Duration::from_secs(1),
        cancel,
    )
    .await
    .unwrap();
    assert!(
        report
            .candidates
            .iter()
            .all(|candidate| matches!(candidate.outcome, CandidateOutcome::Cancelled))
    );
}
#[test]
fn canonical_digest_is_stable_under_object_key_order_and_changes_with_constraints() {
    let first = input();
    let mut second = first.clone();
    second.canonical_context = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
    assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    second.constraints.push("changed constraint".into());
    assert_ne!(first.digest().unwrap(), second.digest().unwrap());
    assert_eq!(first.digest().unwrap().len(), 64);
}
