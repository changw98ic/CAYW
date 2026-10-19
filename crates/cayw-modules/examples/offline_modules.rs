use cayw_modules::{evaluation::*, observer::*, strongflow::*, *};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

struct OfflineCandidate(String);
#[async_trait]
impl FusionCandidateSource for OfflineCandidate {
    fn provider_id(&self) -> &str {
        &self.0
    }
    async fn answer(
        &self,
        _input: Arc<FusionInput>,
        _cancel: CancellationToken,
    ) -> CandidateResponse {
        CandidateResponse::Answer {
            value: json!({"answer":"offline fixture"}),
            usage: None,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(WorkflowPlan {
        id: "demo".into(),
        steps: vec![PlanStep {
            id: "test".into(),
            depends_on: Default::default(),
            acceptance_checks: ["offline".into()].into(),
        }],
    })?;
    let loaded = registry.load_native(
        flow.clone(),
        ModuleContext::new(["strongflow.ready".into()], []),
    )?;
    let mut binding = SessionBinding::strongflow("source-session".into(), &loaded)?;
    println!(
        "ready: {}",
        registry
            .call(
                "strongflow",
                "strongflow.ready",
                json!({"workflow_id":"demo"}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await?
    );
    flow.start_step("demo", "test")?;
    // A real host atomically stores the returned state with this binding after
    // each plan/status/evidence change and before unloading the module.
    let saved_workflows = binding.checkpoint_strongflow(&registry)?;
    // Deliberately no acceptance evidence: this example has not executed a build.
    let (observer, mut reader) = Observer::channel(ObserverConfig::default())?;
    observer.publish(Observation {
        kind: "demo.started".into(),
        fields: [(
            "session".into(),
            ObservedField {
                value: json!(binding.session_id()),
                sensitivity: Sensitivity::Private,
            },
        )]
        .into(),
    });
    println!(
        "observation: {}",
        serde_json::to_string(&reader.recv().await)?
    );
    let input = FusionInput {
        question: "Show identical offline inputs".into(),
        canonical_context: json!({"fixture":true}),
        constraints: vec!["offline".into()],
        answer_schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
    };
    let sources = ["fixture-a", "fixture-b", "fixture-c"]
        .into_iter()
        .map(|id| Arc::new(OfflineCandidate(id.into())) as Arc<dyn FusionCandidateSource>)
        .collect();
    let report = collect_fusion(
        input,
        sources,
        Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await?;
    println!(
        "fixture candidates: {}, input digest: {}",
        report.candidates.len(),
        report.input_digest
    );
    registry.unload("strongflow")?;
    println!(
        "binding availability: {:?}",
        binding.check_available(&registry)
    );
    // This is a demonstration of an explicit host confirmation parameter only.
    // A real runtime must obtain confirmation from its user and persist the fork.
    let fork = binding.request_ordinary_fork("new-session".into(), true)?;
    println!(
        "fork decision (not a persisted session): {}",
        serde_json::to_string(&fork)?
    );
    // Demonstrate a process restart from host-supplied, trusted state. An actual
    // runtime reconciles running effects, then commits this updated binding before
    // admitting work. Recovery never calls the ordinary constructor for this ID.
    let restored = Arc::new(StrongFlow::restore(saved_workflows)?);
    let reloaded = registry.load_native(
        restored,
        ModuleContext::new(["strongflow.ready".into()], []),
    )?;
    binding.resume_strongflow(&registry, &reloaded)?;
    println!(
        "explicit recovery kept mode {:?} and running step {:?}",
        binding.mode(),
        binding.workflow_state().unwrap().workflows["demo"].statuses["test"],
    );
    Ok(())
}
