//! Pre-wiring baseline for Issue 5.
//!
//! This test uses only the public Runtime/Provider API that exists before the
//! collaboration module is wired. It must compile today and fail at runtime
//! until `plan.update` is a real Core capability reached through the original
//! Backend -> Host -> Gate -> audit path.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CapabilityStage, CoreEvent, SessionState};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::{Value, json};
use std::sync::Arc;

const SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect workspace',status='in_progress'}}})"#;

struct Script;
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(&request, SCRIPT, "baseline-plan");
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn exact_plan_scope() -> Value {
    json!({
        "expected_version": 0,
        "steps": [{"id": "inspect", "text": "Inspect workspace", "status": "in_progress"}]
    })
}

#[tokio::test]
async fn model_plan_update_reaches_core_as_a_successful_capability() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script),
        Model::new("fixture", "collaboration-baseline"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("plan.update", exact_plan_scope())],
            ..Default::default()
        },
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "plan the work", CancellationToken::new())
        .await
        .unwrap();
    assert_ne!(view.state, SessionState::NeedsReconciliation);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);

    let records = rt.events(&id, 0, 1000).unwrap();
    assert!(
        records.iter().any(|record| matches!(
            &record.event,
            CoreEvent::CapabilityStarted { call } if call.name == "plan.update"
        )),
        "the model call must enter the original Core capability path"
    );
    let settled: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::CapabilitySettled { call } if call.name == "plan.update" => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(
        settled.len(),
        1,
        "exactly one real Core settlement expected"
    );
    let call = settled[0];
    assert_eq!(call.stage, CapabilityStage::Settled);
    match call.outcome.as_ref() {
        Some(CapabilityOutcome::Completed { result }) => {
            assert_eq!(result["version"], json!(1));
            assert_eq!(result["previous_version"], json!(0));
            assert_eq!(result["source"], json!("model"));
        }
        other => {
            panic!("model plan.update must succeed through the real Core capability, got {other:?}")
        }
    }
}
