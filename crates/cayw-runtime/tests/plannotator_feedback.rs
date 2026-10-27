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

fn reviewed(rt: &Runtime, id: &str) -> PlanFeedbackInput {
    let plan = rt.plan(id).unwrap().expect("reviewable plan");
    PlanFeedbackInput {
        session_id: id.into(),
        expected_task_id: plan.task_id,
        expected_version: plan.version,
        expected_content_sha256: plan.content_sha256,
        decision: FeedbackDecision::Approve,
        comments: vec![],
        summary: None,
        actor: "tester".into(),
    }
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
            "expected_task_id": plan.task_id,
            "expected_version": plan.version,
            "expected_content_sha256": plan.content_sha256,
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
        ..reviewed(&rt, &id)
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
            ..reviewed(&rt, &a)
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
            ..reviewed(&rt2, &b)
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

#[tokio::test]
async fn persist_failure_leaves_no_injectable_memory_state() {
    // F08 durability consistency: submit must not leave memory-only feedback
    // when Core append fails. Disk-first commit — failed API ⇒ not listed,
    // not injected on the next Agent turn.
    let (dir, rt) = setup(PLAN_SCRIPT);
    let id = rt.create(None).unwrap().id;
    start(&rt, &id).await.unwrap().unwrap();
    assert!(rt.plan(&id).unwrap().is_some());

    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_plannotator_persist BEFORE INSERT ON core_events          WHEN json_extract(NEW.record,'$.event.type')='host'           AND json_extract(NEW.record,'$.event.kind')='plannotator_feedback_recorded'          BEGIN SELECT RAISE(ABORT,'injected plannotator persist failure'); END;",
    )
    .unwrap();

    let error = rt
        .submit_plan_feedback(PlanFeedbackInput {
            session_id: id.clone(),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![cayw_plannotator::AnnotationComment {
                comment_id: "c-fail".into(),
                body: "must not inject after persist failure".into(),
                anchor: None,
                created_at_ms: 1,
            }],
            summary: Some("persist-fail".into()),
            ..reviewed(&rt, &id)
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected plannotator persist failure"),
        "submit must surface persist failure, got: {error}"
    );

    // No durable record and no injectable memory state.
    let listed = rt.plannotator_feedback(&id).unwrap();
    assert!(
        listed.is_empty(),
        "failed persist must not leave feedback in memory/list: {listed:?}"
    );

    db.execute_batch("DROP TRIGGER fail_plannotator_persist;")
        .unwrap();

    // Follow-up attempt must not see a cayw-plannotator-feedback section.
    let capture = Arc::new(Capture::new("return 1"));
    let rt2 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        capture.clone(),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();
    assert!(rt2.plannotator_feedback(&id).unwrap().is_empty());
    rt2.prompt(
        &id,
        "follow up after failed persist",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let seen = capture.seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "provider must see follow-up messages");
    let injected = seen.iter().any(|turn| {
        turn.iter().any(|message| match message {
            Message::System(system) => system.sections.contains_key(FEEDBACK_SECTION_KEY),
            _ => false,
        })
    });
    assert!(
        !injected,
        "failed persist must not inject {FEEDBACK_SECTION_KEY}; seen={seen:?}"
    );
}

const TWO_PLANS: &str = r#"
    cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect workspace',status='in_progress'},{id='edit',text='Apply edit',status='pending'}}})
    return cayw.call('plan.update',{expected_version=1,steps={{id='inspect',text='Inspect workspace',status='completed'},{id='edit',text='Apply reviewed edit',status='in_progress'}}})
"#;

async fn pending_update(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                assert_eq!(approval.input.capability, "plan.update");
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second plan update requests approval")
}

#[tokio::test]
async fn reviewed_plan_rejects_stale_task_version_and_hash_without_recording_feedback() {
    let (_dir, rt) = setup(TWO_PLANS);
    let id = rt.create(None).unwrap().id;
    let running = start(&rt, &id);
    let approval = pending_update(&rt, &id).await;
    let old = reviewed(&rt, &id);
    assert_eq!(old.expected_version, 1);
    rt.approve(
        &id,
        &approval.id,
        cayw_runtime::ApprovalChoice::AllowOnce,
        "tester",
    )
    .unwrap();
    running.await.unwrap().unwrap();
    let latest = reviewed(&rt, &id);
    assert_eq!(latest.expected_version, 2);
    assert_ne!(latest.expected_content_sha256, old.expected_content_sha256);
    let mut wrong_task = latest.clone();
    wrong_task.expected_task_id = "other-task".into();
    let mut wrong_hash = latest.clone();
    wrong_hash.expected_content_sha256 = "0".repeat(64);
    for stale in [old, wrong_task, wrong_hash] {
        let error = rt.submit_plan_feedback(stale).unwrap_err();
        assert!(
            error.to_string().contains("plan feedback version conflict"),
            "{error}"
        );
    }
    assert!(rt.plannotator_feedback(&id).unwrap().is_empty());
    let saved = rt.submit_plan_feedback(latest.clone()).unwrap();
    assert_eq!(saved.target.file.version, latest.expected_version);
    assert_eq!(
        saved.target.file.content_sha256,
        latest.expected_content_sha256
    );
    assert_eq!(
        saved.target.file.path,
        format!("plan:{}", latest.expected_task_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_plan_update_and_feedback_preserve_reviewed_identity_across_runtimes() {
    let (dir, rt) = setup(TWO_PLANS);
    let id = rt.create(None).unwrap().id;
    let running = start(&rt, &id);
    let approval = pending_update(&rt, &id).await;
    let old = reviewed(&rt, &id);
    let peer = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script("return 1")),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let submit_barrier = barrier.clone();
    let submit_peer = peer.clone();
    let submitted = tokio::task::spawn_blocking(move || {
        submit_barrier.wait();
        submit_peer.submit_plan_feedback(old)
    });
    let update_rt = rt.clone();
    let update_id = id.clone();
    let update = tokio::task::spawn_blocking(move || {
        barrier.wait();
        update_rt.approve(
            &update_id,
            &approval.id,
            cayw_runtime::ApprovalChoice::AllowOnce,
            "tester",
        )
    });
    update.await.unwrap().unwrap();
    running.await.unwrap().unwrap();
    let result = submitted.await.unwrap();
    let plan = rt.plan(&id).unwrap().unwrap();
    assert_eq!(plan.version, 2);
    match result {
        Ok(feedback) => {
            assert_eq!(feedback.target.file.version, 1);
            assert_ne!(feedback.target.file.content_sha256, plan.content_sha256);
            let feedback_cursor = rt
                .events(&id, 0, 1000)
                .unwrap()
                .into_iter()
                .find_map(|record| match &record.event {
                    cayw_core::CoreEvent::Host { kind, data }
                        if kind == "plannotator_feedback_recorded"
                            && data.get("decision").unwrap_or(data)["feedback_id"]
                                == feedback.feedback_id =>
                    {
                        Some(record.cursor)
                    }
                    _ => None,
                })
                .expect("accepted feedback is durably recorded");
            assert!(
                feedback_cursor < plan.cursor,
                "old-plan feedback cannot commit after the new plan"
            );
        }
        Err(error) => assert!(
            error.to_string().contains("plan feedback version conflict"),
            "{error}"
        ),
    }
    let mut stale = reviewed(&peer, &id);
    stale.expected_version = 1;
    assert!(
        peer.submit_plan_feedback(stale)
            .unwrap_err()
            .to_string()
            .contains("version conflict")
    );
}
