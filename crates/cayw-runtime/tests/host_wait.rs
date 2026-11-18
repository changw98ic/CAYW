//! Real Runtime human-wait boundaries for `user.ask`.
//!
//! The default CodeMode compute budget is 5 seconds of active VM time, and an
//! awaited Host call pauses that clock. These tests use the real Runtime, the
//! real Host gate and a fixed offline provider:
//! - a human answer after 6 seconds still lands and the run completes;
//! - an explicit `host.call_timeout` does not time out a human answer;
//! - the Runtime's absolute `max_run_time` still ends an unanswered wait even
//!   though the VM compute clock is paused.
//!
//! No assertion depends on a fragile upper bound. The 6-second test waits past
//! the compute budget by construction; the other tests await the actual task
//! and only use a generous safety timeout.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_runtime::{
    QuestionState, QuestionView, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskState,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;

const SCRIPT: &str = r#"return cayw.call('user.ask',{question='Which target should I use?',options={'staging','production'}})"#;

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.0,
            &format!("tool-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn setup(config: RuntimeConfig) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let runtime = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(SCRIPT)),
        Model::new("fixture", "host-wait"),
        config,
    )
    .unwrap();
    (dir, runtime)
}

fn ask_permission() -> ScopedPermission {
    ScopedPermission::exact(
        "user.ask",
        json!({"question":"Which target should I use?","options":["staging","production"]}),
    )
}

async fn pending(rt: &Runtime, id: &str) -> QuestionView {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(question) = rt.pending_questions(id).unwrap().into_iter().next() {
                return question;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the question must become pending")
}

fn settled_ask(rt: &Runtime, id: &str) -> Option<CapabilityOutcome> {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|record| match record.event {
            CoreEvent::CapabilitySettled { call } if call.name == "user.ask" => call.outcome,
            _ => None,
        })
}

fn host_count(rt: &Runtime, id: &str, wanted: &str) -> usize {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|record| matches!(&record.event, CoreEvent::Host { kind, .. } if kind == wanted))
        .count()
}

fn start(
    rt: &Runtime,
    id: &str,
) -> tokio::task::JoinHandle<Result<cayw_runtime::SessionView, RuntimeError>> {
    let rt = rt.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        rt.prompt(&id, "collaborate", CancellationToken::new())
            .await
    })
}

fn assert_known_ended_wait(rt: &Runtime, id: &str, question_id: &str) -> TaskState {
    let view = rt.get(id).unwrap();
    assert_eq!(
        view.state,
        SessionState::Idle,
        "a known stop must settle back to Idle"
    );
    assert!(
        view.unresolved_capabilities.is_empty(),
        "no capability may remain unresolved: {:?}",
        view.unresolved_capabilities
    );
    assert!(
        matches!(settled_ask(rt, id), Some(CapabilityOutcome::Failed { .. })),
        "the unanswered ask must settle as a known failure, not an unknown effect"
    );
    let questions = rt.questions(id).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].id, question_id);
    assert_eq!(questions[0].state, QuestionState::Expired);
    assert!(questions[0].answer.is_none());
    let task_state = rt.tasks(id).unwrap()[0].state;
    assert_ne!(
        task_state,
        TaskState::OutcomeUnknown,
        "ending the wait must not leave an unknown task effect"
    );
    assert_eq!(host_count(rt, id, "collaboration_question_asked"), 1);
    assert_eq!(host_count(rt, id, "collaboration_question_answered"), 0);
    assert!(matches!(
        rt.answer_question(id, question_id, "staging", "host-late"),
        Err(RuntimeError::StaleApproval)
    ));
    task_state
}

#[tokio::test]
async fn default_compute_timeout_does_not_end_a_six_second_human_wait() {
    let (_dir, rt) = setup(RuntimeConfig {
        permissions: vec![ask_permission()],
        ..Default::default()
    });
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;

    // The default CodeMode `timeout` is 5 seconds of active VM time. Waiting
    // longer than that for a human must not consume the compute budget.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        !task.is_finished(),
        "the run must still wait for the human after the 5-second compute budget"
    );
    assert_eq!(rt.questions(&id).unwrap()[0].state, QuestionState::Pending);
    assert!(
        rt.pending_questions(&id)
            .unwrap()
            .iter()
            .any(|pending| pending.id == question.id)
    );

    let answered = rt
        .answer_question(&id, &question.id, "production", "host-user")
        .unwrap();
    assert_eq!(answered.state, QuestionState::Answered);
    assert_eq!(answered.answer.as_ref().unwrap().answer, "production");

    let view = timeout(Duration::from_secs(15), task)
        .await
        .expect("the answered run must finish")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert_eq!(rt.questions(&id).unwrap()[0].state, QuestionState::Answered);
    assert!(rt.pending_questions(&id).unwrap().is_empty());
    assert!(matches!(
        settled_ask(&rt, &id),
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert_eq!(host_count(&rt, &id, "collaboration_question_answered"), 1);
}

#[tokio::test]
async fn short_host_call_timeout_leaves_the_question_available_for_an_answer() {
    let mut config = RuntimeConfig {
        permissions: vec![ask_permission()],
        ..Default::default()
    };
    let call_timeout = Duration::from_millis(1500);
    config.host.call_timeout = Some(call_timeout);
    let (_dir, rt) = setup(config);
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;

    tokio::time::sleep(call_timeout).await;
    assert_eq!(rt.questions(&id).unwrap()[0].state, QuestionState::Pending);
    rt.answer_question(&id, &question.id, "staging", "host-user")
        .unwrap();
    let view = timeout(Duration::from_secs(15), task)
        .await
        .expect("the answered run must finish")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert!(matches!(
        settled_ask(&rt, &id),
        Some(CapabilityOutcome::Completed { .. })
    ));
}

#[tokio::test]
async fn waiting_for_a_human_releases_the_only_execution_slot() {
    let (_dir, rt) = setup(RuntimeConfig {
        max_concurrent_runs: 1,
        permissions: vec![ask_permission()],
        ..Default::default()
    });
    let first = rt.create(None).unwrap().id;
    let second = rt.create(None).unwrap().id;
    let left = start(&rt, &first);
    let left_question = pending(&rt, &first).await;
    let right = start(&rt, &second);
    let right_question = pending(&rt, &second).await;
    assert_eq!(
        rt.questions(&first).unwrap()[0].state,
        QuestionState::Pending
    );
    let (left, right) = timeout(Duration::from_secs(15), async {
        rt.answer_question(&second, &right_question.id, "production", "host-user")
            .unwrap();
        let finished_second = right.await.unwrap().unwrap();
        rt.answer_question(&first, &left_question.id, "staging", "host-user")
            .unwrap();
        (left.await.unwrap().unwrap(), finished_second)
    })
    .await
    .unwrap();
    assert_eq!(left.state, SessionState::Idle);
    assert_eq!(right.state, SessionState::Idle);
}

#[tokio::test]
async fn runtime_absolute_deadline_still_ends_a_paused_human_wait() {
    let (_dir, rt) = setup(RuntimeConfig {
        permissions: vec![ask_permission()],
        max_run_time: Some(Duration::from_secs(4)),
        ..Default::default()
    });
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;

    // The Runtime's task deadline still applies while the compute clock is paused.
    let outcome = timeout(Duration::from_secs(20), task)
        .await
        .expect("Runtime max_run_time must end the paused human wait")
        .expect("the worker task must not panic");
    assert!(matches!(outcome, Err(RuntimeError::Budget("time"))));
    assert_eq!(
        assert_known_ended_wait(&rt, &id, &question.id),
        TaskState::Failed
    );
    let tasks = rt.tasks(&id).unwrap();
    assert!(!tasks[0].cancel_requested);
    assert_eq!(
        tasks[0].error.as_deref(),
        Some("task exceeded its time budget")
    );
    let terminals = rt
        .audit(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.task_id.as_deref() == Some(tasks[0].id.as_str()) && event.kind.starts_with("run_")
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind, "run_failed");
}
