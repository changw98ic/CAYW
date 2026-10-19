#![cfg(feature = "strongflow")]
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_modules::{
    ModuleContext,
    strongflow::{PlanStep, StepStatus, StrongFlow, WorkflowPlan},
};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};
struct ModuleProvider;
impl Provider for ModuleProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let m = cayw_runtime::fixture::script_response(
            &request,
            "local description=cayw.call('strongflow.describe',{workflow_id='plan'}); local a=cayw.call('fs.read',{path='a.txt'}); local b=cayw.call('fs.read',{path='a.txt'}); return {description=description,first=a.content,second=b.content}",
            &format!("module-read-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: m,
        }]))
    }
}
fn context() -> ModuleContext {
    ModuleContext::new(["strongflow.describe".into()], [])
}
#[tokio::test]
async fn bound_module_is_really_called_and_no_acceptance_evidence_is_invented() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    std::fs::write(dir.path().join("workspace/a.txt"), "module read bytes").unwrap();
    let mut config = RuntimeConfig {
        permissions: vec![
            ScopedPermission::exact("strongflow.describe", json!({"workflow_id":"plan"})),
            ScopedPermission::exact("fs.read", json!({"path":"a.txt"})),
        ],
        ..Default::default()
    };
    config.host.frozen_reads = true;
    let open = || {
        Runtime::open(
            dir.path().join("sessions.db"),
            dir.path().join("workspace"),
            Arc::new(ModuleProvider),
            Model::new("fixture", "module"),
            config.clone(),
        )
        .unwrap()
    };
    let rt = open();
    let module = Arc::new(StrongFlow::new());
    module
        .create_plan(WorkflowPlan {
            id: "plan".into(),
            steps: vec![PlanStep {
                id: "real-step".into(),
                depends_on: BTreeSet::new(),
                acceptance_checks: BTreeSet::from(["actual-check".into()]),
            }],
        })
        .unwrap();
    rt.load_native(module.clone(), context()).unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_strongflow(&id).unwrap();
    let result = rt
        .prompt(&id, "read only", CancellationToken::new())
        .await
        .unwrap();
    let events = rt.events(&id, 0, 1000).unwrap();
    let capability_start = events
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                cayw_core::CoreEvent::CapabilityStarted { .. }
            )
        })
        .unwrap();
    let checkpoint = events.iter().position(|record| matches!(&record.event, cayw_core::CoreEvent::Host {kind,..} if kind == "module_checkpoint")).unwrap();
    let capability_end = events
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                cayw_core::CoreEvent::CapabilitySettled { .. }
            )
        })
        .unwrap();
    assert!(capability_start < checkpoint && checkpoint < capability_end);
    let reads: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            cayw_core::CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => {
                Some(data)
            }
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["role"], "uncached");
    assert_eq!(reads[1]["role"], "uncached");
    assert_eq!(reads[0]["stamp"], reads[1]["stamp"]);
    assert_ne!(reads[0]["audit_id"], reads[1]["audit_id"]);
    for read in &reads {
        assert_eq!(read["source"], read["origin"]);
    }
    let completed_reads = events.iter().filter(|event| matches!(&event.event,
        cayw_core::CoreEvent::CapabilitySettled { call } if call.name == "fs.read"
            && matches!(&call.outcome, Some(cayw_core::CapabilityOutcome::Completed { result }) if result["content"] == "module read bytes")
    )).count();
    assert_eq!(completed_reads, 2);
    rt.recover(&id).unwrap();
    assert_eq!(
        result
            .binding
            .as_ref()
            .unwrap()
            .workflow_state
            .as_ref()
            .unwrap()["workflows"]["plan"]["statuses"]["real-step"],
        "pending"
    );
    let tool = result
        .messages
        .iter()
        .find(|m| matches!(m, Message::ToolResult(_)))
        .unwrap();
    assert!(tool.text().contains("actual-check"), "{tool:?}");
    assert!(!matches!(tool,Message::ToolResult(r) if r.is_error));
    let snapshot = module.describe("plan").unwrap();
    assert_eq!(snapshot.statuses["real-step"], StepStatus::Pending);
    assert!(snapshot.evidence.is_empty());
    drop(rt);
    let rt = open();
    // Fresh process generations restart at one. Even the same name/version and
    // generation cannot resume the old binding after restart.
    rt.load_native(Arc::new(StrongFlow::new()), context())
        .unwrap();
    assert!(matches!(
        rt.prompt(&id, "must stay frozen", CancellationToken::new())
            .await,
        Err(RuntimeError::Frozen(_))
    ));
}
