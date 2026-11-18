use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::json;
use std::sync::Arc;

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.0,
            &format!("call-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
#[tokio::test]
async fn model_visual_tools_use_original_host_authority_and_session_scope() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let script = "local created=cayw.call('artifacts.put',{artifact_id='model-card',kind='html_card',title='Actual card',body='<p>Recorded</p>'}); assert(created.body==nil); cayw.call('artifacts.update',{artifact_id='model-card',expected_content_version=1,body='<p>Updated</p>'}); local cards=cayw.call('artifacts.list',{}); assert(#cards==1 and cards[1].content_version==2 and cards[1].body==nil); local preview=cayw.call('artifacts.preview',{artifact_id='model-card'}); assert(preview.html=='<p>Updated</p>'); local feedback=cayw.call('plannotator.list',{}); assert(#feedback==0); local inspected=cayw.call('session.run_inspector',{}); return {cards=cards,inspector=inspected}";
    let rt=Runtime::open(dir.path().join("session.sqlite"),&work,Arc::new(Script(script)),Model::new("fixture","visual-tools"),RuntimeConfig {
  permissions:vec![ScopedPermission::exact("artifacts.put",json!({"artifact_id":"model-card","kind":"html_card","title":"Actual card","body":"<p>Recorded</p>"})),ScopedPermission::exact("artifacts.update",json!({"artifact_id":"model-card","expected_content_version":1,"body":"<p>Updated</p>"})),ScopedPermission::exact("artifacts.list",json!({})),ScopedPermission::exact("artifacts.preview",json!({"artifact_id":"model-card"})),ScopedPermission::exact("plannotator.list",json!({})),ScopedPermission::exact("session.run_inspector",json!({}))],..Default::default()
 }).unwrap();
    let session = rt.create(None).unwrap();
    let other = rt.create(None).unwrap();
    rt.prompt(&session.id, "create and inspect", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        rt.tasks(&session.id).unwrap()[0].state,
        TaskState::Completed
    );
    let cards = rt.list_artifacts(&session.id).unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].session_id, session.id);
    assert_eq!(cards[0].body, "<p>Updated</p>");
    assert_eq!(cards[0].content_version, 2);
    assert!(rt.list_artifacts(&other.id).unwrap().is_empty());
    let calls: Vec<_> = rt
        .events(&session.id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 6);
    for call in calls {
        assert!(matches!(
            call.outcome,
            Some(CapabilityOutcome::Completed { .. })
        ));
        assert_eq!(call.lease.session_id, session.id);
        assert!(call.authorization.is_some());
    }
    for definition in rt.capabilities().unwrap().into_iter().filter(|d| {
        [
            "artifacts.put",
            "artifacts.update",
            "artifacts.list",
            "artifacts.preview",
            "plannotator.list",
            "session.run_inspector",
        ]
        .contains(&d.name.as_str())
    }) {
        assert_eq!(definition.parameters["additionalProperties"], false);
        assert!(
            definition.parameters["properties"]
                .get("session_id")
                .is_none()
        );
    }

    drop(rt);
    let reopened = Runtime::open(
        dir.path().join("session.sqlite"),
        &work,
        Arc::new(Script(script)),
        Model::new("fixture", "visual-tools"),
        RuntimeConfig::default(),
    )
    .unwrap();
    assert_eq!(reopened.list_artifacts(&session.id).unwrap(), cards);
    assert_eq!(
        reopened.preview_artifact("model-card").unwrap().html,
        "<p>Updated</p>"
    );
}
