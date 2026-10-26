//! F08 audit remediation: applied plan feedback injects into the next Agent
//! attempt via Runtime/Agent prompt messages and survives Core session reopen.

use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_ipc::RpcHandler;
use cayw_plannotator::{FEEDBACK_SECTION_KEY, FeedbackDecision};
use cayw_runtime::{PlanFeedbackInput, Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::json;
use std::sync::{Arc, Mutex};

const PLAN_SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect workspace',status='in_progress'},{id='edit',text='Apply edit',status='pending'}}})"#;

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, r: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &r,
            self.0,
            &format!("tool-{}", r.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

struct Capture {
    seen: Mutex<Vec<Vec<Message>>>,
    script: &'static str,
}
impl Capture {
    fn new(script: &'static str) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            script,
        }
    }
}
impl Provider for Capture {
    fn stream(&self, r: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.seen.lock().unwrap().push(r.messages.clone());
        let message = cayw_runtime::fixture::script_response(
            &r,
            self.script,
            &format!("tool-{}", r.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn plan_scope() -> serde_json::Value {
    json!({
        "expected_version": 0,
        "steps": [
            {"id":"inspect","text":"Inspect workspace","status":"in_progress"},
            {"id":"edit","text":"Apply edit","status":"pending"}
        ]
    })
}

fn setup(script: &'static str) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(script)),
        Model::new("fixture", "plannotator"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("plan.update", plan_scope())],
            ..Default::default()
        },
    )
    .unwrap();
    (dir, rt)
}

fn start(
    rt: &Runtime,
    id: &str,
) -> tokio::task::JoinHandle<Result<cayw_runtime::SessionView, cayw_runtime::RuntimeError>> {
    let rt = rt.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        rt.prompt(&id, "collaborate", CancellationToken::new())
            .await
    })
}

#[tokio::test]
async fn plan_feedback_persists_and_lists_via_rpc() {
    let (dir, rt) = setup(PLAN_SCRIPT);
    let id = rt.create(None).unwrap().id;
    let view = start(&rt, &id).await.unwrap().unwrap();
    assert_ne!(view.state, cayw_core::SessionState::NeedsReconciliation);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let plan = rt.plan(&id).unwrap().expect("plan");
    assert_eq!(plan.version, 1);

    let submitted = RpcHandler::call(
        &rt,
        "plannotator.submit",
        json!({
            "session_id": id,
            "decision": "deny_with_feedback",
            "summary": "add validation before edit",
            "comments": [{
                "comment_id": "c1",
                "body": "validation belongs in the form handler",
                "created_at_ms": 1
            }]
        }),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(submitted["decision"], "deny_with_feedback");
    assert_eq!(submitted["visible_from_attempt"], 2);

    let listed = RpcHandler::call(
        &rt,
        "plannotator.list",
        json!({"session_id": id}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let peer = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(PLAN_SCRIPT)),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let restored = peer.plannotator_feedback(&id).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].decision, FeedbackDecision::DenyWithFeedback);
    assert_eq!(restored[0].visible_from_attempt, 2);
}

#[tokio::test]
async fn applied_feedback_injects_system_section_on_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let first = Arc::new(Capture::new(PLAN_SCRIPT));
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        first.clone(),
        Model::new("fixture", "plannotator"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("plan.update", plan_scope())],
            ..Default::default()
        },
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    start(&rt, &id).await.unwrap().unwrap();
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert!(rt.plan(&id).unwrap().is_some());

    rt.submit_plan_feedback(PlanFeedbackInput {
        session_id: id.clone(),
        decision: FeedbackDecision::DenyWithFeedback,
        comments: vec![cayw_plannotator::AnnotationComment {
            comment_id: "c1".into(),
            body: "fix validation".into(),
            anchor: None,
            created_at_ms: 1,
        }],
        summary: Some("revise plan".into()),
        actor: "tester".into(),
    })
    .unwrap();

    let second = Arc::new(Capture::new("return 1"));
    let rt2 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        second.clone(),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();
    // Ensure store is hydrated from Core before the follow-up attempt.
    assert_eq!(rt2.plannotator_feedback(&id).unwrap().len(), 1);
    rt2.prompt(&id, "follow up", CancellationToken::new())
        .await
        .unwrap();

    let seen = second.seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "provider must see follow-up messages");
    let injected = seen.iter().any(|turn| {
        turn.iter().any(|message| match message {
            Message::System(system) => system
                .sections
                .get(FEEDBACK_SECTION_KEY)
                .and_then(|v| v.as_ref())
                .is_some_and(|body| {
                    body.contains("deny_with_feedback") && body.contains("fix validation")
                }),
            _ => false,
        })
    });
    assert!(
        injected,
        "next attempt must carry {FEEDBACK_SECTION_KEY} section; seen={seen:?}"
    );
}

#[tokio::test]
async fn restart_does_not_clobber_prior_session_feedback_ids() {
    // Critical F08 audit path: global fb-N + lazy per-Session hydrate let a
    // fresh session after restart allocate the same id and swallow the prior
    // session's restored record. UUID ids + (session, id) restore fix this.
    let (dir, rt) = setup(PLAN_SCRIPT);
    let a = rt.create(None).unwrap().id;
    start(&rt, &a).await.unwrap().unwrap();
    let old = rt
        .submit_plan_feedback(PlanFeedbackInput {
            session_id: a.clone(),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![cayw_plannotator::AnnotationComment {
                comment_id: "c-old".into(),
                body: "must survive restart".into(),
                anchor: None,
                created_at_ms: 1,
            }],
            summary: Some("session-a-old".into()),
            actor: "tester".into(),
        })
        .unwrap();

    // Simulate process restart with a new Runtime on the same Core db.
    let rt2 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(PLAN_SCRIPT)),
        Model::new("fixture", "plannotator"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("plan.update", plan_scope())],
            ..Default::default()
        },
    )
    .unwrap();

    // New session submits BEFORE the old session is hydrated (lazy restore).
    let b = rt2.create(None).unwrap().id;
    start(&rt2, &b).await.unwrap().unwrap();
    let fresh = rt2
        .submit_plan_feedback(PlanFeedbackInput {
            session_id: b.clone(),
            decision: FeedbackDecision::ApproveWithNotes,
            comments: vec![cayw_plannotator::AnnotationComment {
                comment_id: "c-new".into(),
                body: "new session after restart".into(),
                anchor: None,
                created_at_ms: 2,
            }],
            summary: Some("session-b-new".into()),
            actor: "tester".into(),
        })
        .unwrap();
    assert_ne!(
        fresh.feedback_id, old.feedback_id,
        "new allocation must not reuse the durable id from the prior session"
    );

    // Hydrate the prior session afterward — its record must still be present.
    let restored_a = rt2.plannotator_feedback(&a).unwrap();
    assert_eq!(restored_a.len(), 1);
    assert_eq!(restored_a[0].feedback_id, old.feedback_id);
    assert_eq!(
        restored_a[0].summary.as_deref(),
        Some("session-a-old"),
        "old feedback must not be swallowed by a same-id new record"
    );
    let restored_b = rt2.plannotator_feedback(&b).unwrap();
    assert_eq!(restored_b.len(), 1);
    assert_eq!(restored_b[0].feedback_id, fresh.feedback_id);
}
