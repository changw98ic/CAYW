//! Issue 5 collaboration integration tests: model `user.ask` / `plan.update`
//! through the original Host gate, Core-authoritative questions, host-only
//! answers, cross-Runtime durability and plan compare-and-swap.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_ipc::RpcHandler;
use cayw_runtime::{
    ApprovalChoice, PlanStepStatus, QuestionState, QuestionView, Runtime, RuntimeConfig,
    ScopedPermission, TaskState,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const ASK_SCRIPT: &str = r#"return cayw.call('user.ask',{question='Which target should I use?',options={'staging','production'}})"#;

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

fn setup(script: &'static str, permissions: Vec<ScopedPermission>) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(script)),
        Model::new("fixture", "collaboration"),
        RuntimeConfig {
            permissions,
            ..Default::default()
        },
    )
    .unwrap();
    (dir, rt)
}
fn reopen(dir: &tempfile::TempDir, script: &'static str) -> Runtime {
    Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(script)),
        Model::new("fixture", "collaboration"),
        RuntimeConfig::default(),
    )
    .unwrap()
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
async fn pending(rt: &Runtime, id: &str) -> QuestionView {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(question) = rt.pending_questions(id).unwrap().into_iter().next() {
                return question;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("question must become pending")
}
async fn approval(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approval must become pending")
}
fn host_events(rt: &Runtime, id: &str, kind: &str) -> Vec<Value> {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            cayw_core::CoreEvent::Host { kind: actual, data } if actual == kind => Some(data),
            _ => None,
        })
        .collect()
}
fn ask_scope() -> Value {
    json!({"question":"Which target should I use?","options":["staging","production"]})
}

#[tokio::test]
async fn question_round_trip_is_durable_and_answered_by_host() {
    let (_dir, rt) = setup(
        ASK_SCRIPT,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;
    assert_eq!(question.question, "Which target should I use?");
    assert_eq!(question.options, vec!["staging", "production"]);
    assert_eq!(question.state, QuestionState::Pending);
    assert!(question.answer.is_none());
    let view = rt.get(&id).unwrap();
    assert_eq!(
        view.attempt.as_ref().map(|lease| lease.attempt_id.as_str()),
        Some(question.attempt_id.as_str())
    );

    let answered = rt
        .answer_question(&id, &question.id, "staging", "host-user")
        .unwrap();
    assert_eq!(answered.state, QuestionState::Answered);
    assert_eq!(answered.answer.as_ref().unwrap().answer, "staging");
    assert_eq!(answered.answer.as_ref().unwrap().actor, "host-user");
    task.await.unwrap().unwrap();

    assert_eq!(rt.questions(&id).unwrap().len(), 1);
    assert_eq!(rt.questions(&id).unwrap()[0].state, QuestionState::Answered);
    assert!(rt.pending_questions(&id).unwrap().is_empty());
    assert!(rt.plan(&id).unwrap().is_none());
    // Answering is not a permission decision and creates no grant.
    assert!(rt.grants(&id).unwrap().is_empty());
    assert_eq!(
        host_events(&rt, &id, "collaboration_question_asked").len(),
        1
    );
    assert_eq!(
        host_events(&rt, &id, "collaboration_question_answered").len(),
        1
    );
    // The original call pair stayed on the ordinary capability audit path.
    let names: Vec<_> = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            cayw_core::CoreEvent::CapabilityStarted { call } => Some(call.name),
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|name| name == "user.ask"));
}

#[tokio::test]
async fn duplicate_answers_are_idempotent_or_rejected_and_never_grant() {
    let (_dir, rt) = setup(
        ASK_SCRIPT,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let id = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;
    rt.answer_question(&id, &question.id, "staging", "host-a")
        .unwrap();
    // Repeating the identical answer is idempotent, not a second answer event.
    let again = rt
        .answer_question(&id, &question.id, "staging", "host-b")
        .unwrap();
    assert_eq!(again.answer.as_ref().unwrap().answer, "staging");
    assert!(matches!(
        rt.answer_question(&id, &question.id, "production", "host-c"),
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
    // A question id is scoped to its own session.
    assert!(matches!(
        rt.answer_question(&other, &question.id, "staging", "host-a"),
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
    task.await.unwrap().unwrap();
    assert!(matches!(
        rt.answer_question(&id, &question.id, "staging", "host-late"),
        Err(cayw_runtime::RuntimeError::StaleApproval)
    ));
    assert_eq!(
        host_events(&rt, &id, "collaboration_question_answered").len(),
        1
    );
    assert!(rt.grants(&id).unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_question_keeps_its_fact_and_rejects_late_answer() {
    let (_dir, rt) = setup(
        ASK_SCRIPT,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "collaborate").unwrap();
    let runner = rt.clone();
    let task_id = admitted.id.clone();
    let task = tokio::spawn(async move { runner.resume(&task_id, CancellationToken::new()).await });
    let question = pending(&rt, &id).await;
    rt.cancel(&rt.task(&admitted.id).unwrap().cancel_target())
        .unwrap();
    let _ = task.await.unwrap();
    let view = rt.get(&id).unwrap();
    assert_ne!(view.state, cayw_core::SessionState::NeedsReconciliation);
    // The known wait failure does not erase the original question fact.
    assert_eq!(
        host_events(&rt, &id, "collaboration_question_asked").len(),
        1
    );
    assert_eq!(
        host_events(&rt, &id, "collaboration_question_answered").len(),
        0
    );
    assert_eq!(rt.questions(&id).unwrap()[0].state, QuestionState::Expired);
    assert!(matches!(
        rt.answer_question(&id, &question.id, "staging", "host-late"),
        Err(cayw_runtime::RuntimeError::StaleApproval)
    ));
    assert_ne!(rt.tasks(&id).unwrap()[0].state, TaskState::OutcomeUnknown);
}

#[tokio::test]
async fn denied_approval_never_persists_a_question() {
    let (_dir, rt) = setup(ASK_SCRIPT, vec![]);
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let request = approval(&rt, &id).await;
    assert_eq!(request.input.capability, "user.ask");
    assert!(rt.questions(&id).unwrap().is_empty());
    rt.approve(&id, &request.id, ApprovalChoice::Deny, "host-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert!(rt.questions(&id).unwrap().is_empty());
    assert!(host_events(&rt, &id, "collaboration_question_asked").is_empty());
    assert!(rt.pending_questions(&id).unwrap().is_empty());
    assert!(matches!(
        rt.answer_question(&id, "missing-question", "staging", "host"),
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
    assert!(
        rt.grants(&id)
            .unwrap()
            .iter()
            .all(|grant| grant.state != "active")
    );
}

#[tokio::test]
async fn permission_expiry_stops_the_original_wait_as_known_failure() {
    let (dir, rt) = setup(ASK_SCRIPT, vec![]);
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let request = approval(&rt, &id).await;
    rt.approve(&id, &request.id, ApprovalChoice::AllowTask, "host-user")
        .unwrap();
    let question = pending(&rt, &id).await;
    let grant = rt
        .grants(&id)
        .unwrap()
        .into_iter()
        .find(|grant| grant.state == "active" && grant.capability == "user.ask")
        .expect("an active exact grant admits the ask");
    // Expire only the persisted grant while the question is still waiting. The
    // response-time permission check must stop the original call as a known
    // failure, not as an unknown effect.
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE runtime_grants SET expires=0 WHERE id=?1 AND session=?2",
            rusqlite::params![grant.id, id],
        )
        .unwrap(),
        1
    );
    let view = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("expired permission must stop the wait")
        .unwrap()
        .unwrap();
    assert_ne!(view.state, cayw_core::SessionState::NeedsReconciliation);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert_eq!(
        host_events(&rt, &id, "collaboration_question_asked").len(),
        1
    );
    assert!(rt.questions(&id).unwrap()[0].answer.is_none());
    assert!(matches!(
        rt.answer_question(&id, &question.id, "staging", "host-late"),
        Err(cayw_runtime::RuntimeError::StaleApproval)
    ));
}

#[tokio::test]
async fn cross_runtime_answer_uses_only_core_lease_queue_and_coordination() {
    let (dir, rt) = setup(
        ASK_SCRIPT,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;
    // This Runtime never registered the active run in its own map. The answer is
    // accepted only from Core's current lease and the shared queue row.
    let peer = reopen(&dir, ASK_SCRIPT);
    assert_eq!(
        peer.pending_questions(&id).unwrap()[0].id,
        question.id,
        "Core events must list the question for another Runtime connection"
    );
    let answered = peer
        .answer_question(&id, &question.id, "production", "peer-host")
        .unwrap();
    assert_eq!(answered.state, QuestionState::Answered);
    task.await.unwrap().unwrap();
    assert_eq!(
        peer.questions(&id).unwrap()[0].state,
        QuestionState::Answered
    );
    assert_eq!(
        peer.events(&id, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|record| {
                matches!(&record.event, cayw_core::CoreEvent::Host { kind, .. }
                    if kind == "collaboration_question_answered")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn plan_update_uses_per_task_version_cas_and_validates_steps() {
    // The uncaught stale second call ends the cell with a known failure. The
    // first version stays the only durable plan without unknown side effects.
    const SCRIPT: &str = r#"
local first = cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect workspace',status='in_progress'},{id='edit',text='Apply edit',status='pending'}}})
return cayw.call('plan.update',{expected_version=0,steps={{id='conflict',text='Must not commit',status='pending'}}})
"#;
    let first_scope = json!({
        "expected_version":0,
        "steps":[
            {"id":"inspect","text":"Inspect workspace","status":"in_progress"},
            {"id":"edit","text":"Apply edit","status":"pending"}
        ]
    });
    let conflict_scope = json!({
        "expected_version":0,
        "steps":[{"id":"conflict","text":"Must not commit","status":"pending"}]
    });
    let (_dir, rt) = setup(
        SCRIPT,
        vec![
            ScopedPermission::exact("plan.update", first_scope),
            ScopedPermission::exact("plan.update", conflict_scope),
        ],
    );
    let id = rt.create(None).unwrap().id;
    let first = start(&rt, &id).await.unwrap().unwrap();
    assert_ne!(first.state, cayw_core::SessionState::NeedsReconciliation);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let plan = rt.plan(&id).unwrap().expect("one accepted plan");
    assert_eq!(plan.version, 1);
    assert_eq!(plan.previous_version, 0);
    assert_eq!(plan.source, "model");
    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[0].status, PlanStepStatus::InProgress);
    // The conflicting expected_version=0 call wrote no second plan event.
    assert_eq!(host_events(&rt, &id, "collaboration_plan_updated").len(), 1);
    // A new task starts from an empty per-task plan again.
    let second = start(&rt, &id);
    second.await.unwrap().unwrap();
    assert_eq!(rt.plan(&id).unwrap().unwrap().version, 1);
}

#[tokio::test]
async fn plan_update_allows_parallel_in_progress_steps_with_the_same_version_cas() {
    let script: &'static str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='left',text='Run left work',status='in_progress'},{id='right',text='Run right work',status='in_progress'},{id='merge',text='Merge results',status='pending'}}})"#;
    let scope = json!({
        "expected_version": 0,
        "steps": [
            {"id":"left", "text":"Run left work", "status":"in_progress"},
            {"id":"right", "text":"Run right work", "status":"in_progress"},
            {"id":"merge", "text":"Merge results", "status":"pending"},
        ]
    });
    let (_dir, rt) = setup(script, vec![ScopedPermission::exact("plan.update", scope)]);
    let id = rt.create(None).unwrap().id;
    let view = start(&rt, &id).await.unwrap().unwrap();
    assert_ne!(view.state, cayw_core::SessionState::NeedsReconciliation);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let plan = rt.plan(&id).unwrap().expect("one accepted plan");
    assert_eq!(plan.version, 1);
    assert_eq!(plan.previous_version, 0);
    assert_eq!(
        plan.steps
            .iter()
            .map(|step| (step.id.as_str(), step.status))
            .collect::<Vec<_>>(),
        vec![
            ("left", PlanStepStatus::InProgress),
            ("right", PlanStepStatus::InProgress),
            ("merge", PlanStepStatus::Pending)
        ]
    );
    assert_eq!(host_events(&rt, &id, "collaboration_plan_updated").len(), 1);
}

#[tokio::test]
async fn invalid_plan_arguments_never_write_a_plan_event() {
    const SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='same',text='A',status='in_progress'},{id='same',text='B',status='in_progress'}}})"#;
    let scope = json!({
        "expected_version":0,
        "steps":[
            {"id":"same","text":"A","status":"in_progress"},
            {"id":"same","text":"B","status":"in_progress"}
        ]
    });
    let (_dir, rt) = setup(SCRIPT, vec![ScopedPermission::exact("plan.update", scope)]);
    let id = rt.create(None).unwrap().id;
    let view = start(&rt, &id).await.unwrap().unwrap();
    assert_ne!(view.state, cayw_core::SessionState::NeedsReconciliation);
    assert!(rt.plan(&id).unwrap().is_none());
    assert!(host_events(&rt, &id, "collaboration_plan_updated").is_empty());
    // The invalid call is a known, non-executed failure.
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
}

#[tokio::test]
async fn catalog_exposes_only_model_collaboration_and_rpc_answers() {
    let (_dir, rt) = setup(
        ASK_SCRIPT,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let names: Vec<_> = rt
        .capabilities()
        .unwrap()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.iter().any(|name| name == "user.ask"));
    assert!(names.iter().any(|name| name == "plan.update"));
    assert!(
        names
            .iter()
            .all(|name| !name.contains("answer") && !name.contains("grant")),
        "the model must have no answer/grant capability: {names:?}"
    );

    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;
    let answered = RpcHandler::call(
        &rt,
        "question.answer",
        json!({"session_id":id,"request_id":question.id,"answer":"production"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(answered["state"], "answered");
    assert_eq!(answered["answer"]["actor"], "rpc-user");
    task.await.unwrap().unwrap();
    let listed = RpcHandler::call(
        &rt,
        "session.questions",
        json!({"session_id":id}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["state"], "answered");
    let plan = RpcHandler::call(
        &rt,
        "session.plan",
        json!({"session_id":id}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(plan.is_null());
    let missing = RpcHandler::call(
        &rt,
        "question.answer",
        json!({"session_id":id,"request_id":"not-a-question","answer":"x"}),
        CancellationToken::new(),
    )
    .await;
    assert!(missing.is_err(), "unknown questions must be rejected");
}
