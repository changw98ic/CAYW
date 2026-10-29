//! Regressions and verification for RT-14 and RT-09:
//! - RT-14: user.ask preserves separate namespaces for Task queue fence and Core Session attempt lease fence;
//!   sequential tasks in the same session can both ask and receive answers; old questions / fenced tasks reject answers.
//! - RT-09: collaboration capabilities validate and prepare inputs once at invoke before Host approval;
//!   invalid inputs are rejected before approval; valid calls pass Host and enforce real-time CAS and lease checks.

use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_runtime::{
    QuestionState, QuestionView, Runtime, RuntimeConfig, ScopedPermission, TaskState,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct DynamicScript<F>(F);
impl<F: Fn(&str) -> &'static str + Send + Sync> Provider for DynamicScript<F> {
    fn stream(&self, r: ModelRequest, _: CancellationToken) -> AssistantStream {
        let user_prompt = r
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => content.iter().find_map(|c| match c {
                    cayw_ai::Content::Text { text } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or("");
        let script = (self.0)(user_prompt);
        let message = cayw_runtime::fixture::script_response(
            &r,
            script,
            &format!("tool-{}", r.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn setup_dynamic<F: Fn(&str) -> &'static str + Send + Sync + 'static>(
    f: F,
    permissions: Vec<ScopedPermission>,
) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(DynamicScript(f)),
        Model::new("fixture", "collaboration"),
        RuntimeConfig {
            permissions,
            ..Default::default()
        },
    )
    .unwrap();
    (dir, rt)
}

async fn wait_for_pending_question(rt: &Runtime, id: &str) -> QuestionView {
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

#[tokio::test]
async fn rt14_sequential_tasks_in_same_session_both_answer_user_ask() {
    const TASK1_SCRIPT: &str =
        r#"return cayw.call('user.ask',{question='Target for task 1?',options={'alpha','beta'}})"#;
    const TASK2_SCRIPT: &str =
        r#"return cayw.call('user.ask',{question='Target for task 2?',options={'gamma','delta'}})"#;

    let (_dir, rt) = setup_dynamic(
        |prompt| {
            if prompt.contains("task2") {
                TASK2_SCRIPT
            } else {
                TASK1_SCRIPT
            }
        },
        vec![
            ScopedPermission::exact(
                "user.ask",
                json!({"question":"Target for task 1?","options":["alpha","beta"]}),
            ),
            ScopedPermission::exact(
                "user.ask",
                json!({"question":"Target for task 2?","options":["gamma","delta"]}),
            ),
        ],
    );

    let session = rt.create(None).unwrap().id;

    // --- Task 1 ---
    let runner = rt.clone();
    let s_id = session.clone();
    let task1_handle = tokio::spawn(async move {
        runner
            .prompt(&s_id, "prompt for task1", CancellationToken::new())
            .await
    });

    let q1 = wait_for_pending_question(&rt, &session).await;
    assert_eq!(q1.question, "Target for task 1?");
    assert_eq!(q1.state, QuestionState::Pending);

    let answered1 = rt
        .answer_question(&session, &q1.id, "alpha", "host-user")
        .expect("Task 1 question should be answerable");
    assert_eq!(answered1.state, QuestionState::Answered);
    assert_eq!(answered1.answer.unwrap().answer, "alpha");

    task1_handle
        .await
        .unwrap()
        .expect("Task 1 must complete successfully");

    // Verify session attempt fence has incremented, but next task in queue will have task fence = 1
    let tasks = rt.tasks(&session).unwrap();
    assert_eq!(tasks.len(), 1);

    // --- Task 2 in the SAME Session ---
    let runner = rt.clone();
    let s_id = session.clone();
    let task2_handle = tokio::spawn(async move {
        runner
            .prompt(&s_id, "prompt for task2", CancellationToken::new())
            .await
    });

    let q2 = wait_for_pending_question(&rt, &session).await;
    assert_eq!(q2.question, "Target for task 2?");
    assert_eq!(q2.state, QuestionState::Pending);

    // Answering old question q1 should be rejected as StaleApproval
    let stale_ans = rt.answer_question(&session, &q1.id, "alpha", "host-user");
    assert!(
        matches!(stale_ans, Err(cayw_runtime::RuntimeError::StaleApproval)),
        "Old question from prior attempt must be rejected"
    );

    let answered2 = rt
        .answer_question(&session, &q2.id, "gamma", "host-user")
        .expect(
            "Task 2 question must be answerable despite task fence != session attempt lease fence",
        );
    assert_eq!(answered2.state, QuestionState::Answered);
    assert_eq!(answered2.answer.unwrap().answer, "gamma");

    task2_handle
        .await
        .unwrap()
        .expect("Task 2 must complete successfully");

    let all_questions = rt.questions(&session).unwrap();
    assert_eq!(all_questions.len(), 2);
    assert_eq!(all_questions[0].state, QuestionState::Answered);
    assert_eq!(all_questions[1].state, QuestionState::Answered);
    assert!(rt.pending_questions(&session).unwrap().is_empty());
}

#[tokio::test]
async fn rt14_task_cancellation_and_recovery_reject_answers() {
    const ASK_SCRIPT: &str =
        r#"return cayw.call('user.ask',{question='Deploy now?',options={'yes','no'}})"#;

    let (_dir, rt) = setup_dynamic(
        |_| ASK_SCRIPT,
        vec![ScopedPermission::exact(
            "user.ask",
            json!({"question":"Deploy now?","options":["yes","no"]}),
        )],
    );

    let session = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&session, "deploy prompt").unwrap();
    let runner = rt.clone();
    let task_id = admitted.id.clone();
    let handle =
        tokio::spawn(async move { runner.resume(&task_id, CancellationToken::new()).await });

    let q = wait_for_pending_question(&rt, &session).await;
    assert_eq!(q.state, QuestionState::Pending);

    // Cancel the running task
    rt.cancel(&rt.task(&admitted.id).unwrap().cancel_target())
        .unwrap();
    let _ = handle.await.unwrap();

    // After cancellation, question state is Expired
    let questions = rt.questions(&session).unwrap();
    assert_eq!(questions[0].state, QuestionState::Expired);

    // Answering an expired/cancelled question must return StaleApproval
    let res = rt.answer_question(&session, &q.id, "yes", "operator");
    assert!(matches!(
        res,
        Err(cayw_runtime::RuntimeError::StaleApproval)
    ));
}

#[tokio::test]
async fn rt09_invalid_question_rejected_at_invoke_before_approval() {
    // Empty question string fails semantic check validate_ask
    const INVALID_ASK_SCRIPT: &str = r#"return cayw.call('user.ask',{question='',options={'a'}})"#;

    // No permissions pre-granted, so a valid call would create a pending approval
    let (_dir, rt) = setup_dynamic(|_| INVALID_ASK_SCRIPT, vec![]);
    let session = rt.create(None).unwrap().id;

    let res = rt
        .prompt(&session, "invalid ask", CancellationToken::new())
        .await;
    // Task should fail with validation error, NOT block on approval
    assert!(res.is_err() || rt.get(&session).unwrap().pending_approvals.is_empty());
    assert!(
        rt.get(&session).unwrap().pending_approvals.is_empty(),
        "No approval request should be created for semantically invalid question"
    );
    assert!(rt.questions(&session).unwrap().is_empty());
}

#[tokio::test]
async fn rt09_duplicate_step_id_rejected_at_invoke_before_approval() {
    // Duplicate step id "step-1" fails semantic check check_steps
    const INVALID_PLAN_SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='step-1',text='First',status='pending'},{id='step-1',text='Duplicate',status='pending'}}})"#;

    let (_dir, rt) = setup_dynamic(|_| INVALID_PLAN_SCRIPT, vec![]);
    let session = rt.create(None).unwrap().id;

    let _ = rt
        .prompt(&session, "invalid plan", CancellationToken::new())
        .await;
    assert!(
        rt.get(&session).unwrap().pending_approvals.is_empty(),
        "No approval request should be created for duplicate step ids"
    );
    assert!(rt.plan(&session).unwrap().is_none());
}

#[tokio::test]
async fn rt09_valid_plan_passes_host_and_enforces_cas_version() {
    const PLAN_CAS_SCRIPT: &str = r#"
        local p1 = cayw.call('plan.update',{expected_version=0,steps={{id='s1',text='Step 1',status='pending'}}})
        -- CAS conflict: expecting version 0 when current is 1
        local ok, err = pcall(function()
            return cayw.call('plan.update',{expected_version=0,steps={{id='s1',text='Step 1 updated',status='completed'}}})
        end)
        if ok then error('expected version conflict') end
        -- Valid CAS: expecting version 1
        return cayw.call('plan.update',{expected_version=1,steps={{id='s1',text='Step 1 updated',status='completed'}}})
    "#;

    let (_dir, rt) = setup_dynamic(
        |_| PLAN_CAS_SCRIPT,
        vec![
            ScopedPermission::exact(
                "plan.update",
                json!({"expected_version":0,"steps":[{"id":"s1","text":"Step 1","status":"pending"}]}),
            ),
            ScopedPermission::exact(
                "plan.update",
                json!({"expected_version":0,"steps":[{"id":"s1","text":"Step 1 updated","status":"completed"}]}),
            ),
            ScopedPermission::exact(
                "plan.update",
                json!({"expected_version":1,"steps":[{"id":"s1","text":"Step 1 updated","status":"completed"}]}),
            ),
        ],
    );

    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "run plan cas", CancellationToken::new())
        .await
        .unwrap();

    let plan = rt.plan(&session).unwrap().expect("Plan should be recorded");
    assert_eq!(plan.version, 2);
    assert_eq!(plan.previous_version, 1);
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].text, "Step 1 updated");
}

#[tokio::test]
async fn rt14_task_crash_recovery_fence_increment_rejects_answers() {
    const ASK_SCRIPT: &str =
        r#"return cayw.call('user.ask',{question='Deploy target?',options={'blue','green'}})"#;

    let (_dir, rt) = setup_dynamic(
        |_| ASK_SCRIPT,
        vec![ScopedPermission::exact(
            "user.ask",
            json!({"question":"Deploy target?","options":["blue","green"]}),
        )],
    );

    let session = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&session, "deploy prompt").unwrap();
    let runner = rt.clone();
    let task_id = admitted.id.clone();
    let handle =
        tokio::spawn(async move { runner.resume(&task_id, CancellationToken::new()).await });

    let q = wait_for_pending_question(&rt, &session).await;
    assert_eq!(q.state, QuestionState::Pending);

    // Abort the running worker future to simulate worker crash/interruption
    handle.abort();
    let _ = handle.await;
    rt.wait_idle(&session).await;

    // Recover session: bumps task queue fence and cleans unconfirmed worker state
    rt.recover(&session).expect("Session recovery must succeed");

    // Old question must now be Expired because the task queue fence was incremented / worker fenced
    let questions = rt.questions(&session).unwrap();
    assert_eq!(questions[0].state, QuestionState::Expired);

    // Answering old question must be strictly rejected with StaleApproval
    let res = rt.answer_question(&session, &q.id, "blue", "operator");
    assert!(
        matches!(res, Err(cayw_runtime::RuntimeError::StaleApproval)),
        "Old question must be rejected after recovery fences the task"
    );
}

#[tokio::test]
async fn rt09_plan_update_denied_at_approval_blocks_plan() {
    const PLAN_SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='s1',text='Step 1',status='pending'}}})"#;

    // No permissions pre-granted, requiring approval
    let (_dir, rt) = setup_dynamic(|_| PLAN_SCRIPT, vec![]);
    let session = rt.create(None).unwrap().id;

    let runner = rt.clone();
    let s_id = session.clone();
    let handle = tokio::spawn(async move {
        runner
            .prompt(&s_id, "prompt for plan", CancellationToken::new())
            .await
    });

    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(app) = rt
                .get(&session)
                .unwrap()
                .pending_approvals
                .into_iter()
                .next()
            {
                return app;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approval for plan.update must become pending");

    assert_eq!(approval.input.capability, "plan.update");

    // Deny the approval choice
    rt.approve(
        &session,
        &approval.id,
        cayw_runtime::ApprovalChoice::Deny,
        "reviewer",
    )
    .expect("Approval denial must succeed");

    let _ = handle.await.unwrap();
    assert!(
        rt.plan(&session).unwrap().is_none(),
        "Denied plan.update must not record any plan in Core"
    );
}

#[tokio::test]
async fn rt09_plan_update_cancelled_while_waiting_for_approval_retains_no_plan() {
    const PLAN_SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='s1',text='Step 1',status='pending'}}})"#;

    let (_dir, rt) = setup_dynamic(|_| PLAN_SCRIPT, vec![]);
    let session = rt.create(None).unwrap().id;

    let admitted = rt.enqueue(&session, "plan task").unwrap();
    let runner = rt.clone();
    let task_id = admitted.id.clone();
    let handle =
        tokio::spawn(async move { runner.resume(&task_id, CancellationToken::new()).await });

    // Wait until approval is pending
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rt.get(&session).unwrap();
            if !view.pending_approvals.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approval must become pending");

    // Cancel the task while it is parked waiting for approval
    let target = rt.task(&admitted.id).unwrap().cancel_target();
    rt.cancel(&target).unwrap();
    let _ = handle.await.unwrap();

    let task = rt.task(&admitted.id).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(
        rt.plan(&session).unwrap().is_none(),
        "Cancelled plan.update must not record any plan in Core"
    );
}
