//! Real Runtime human-wait boundaries for `user.ask`.
//!
//! The default CodeMode compute budget is 5 seconds of active VM time, and an
//! awaited Host call pauses that clock. These tests use the real Runtime, the
//! real Host gate and a fixed offline provider:
//! - a human answer after 6 seconds still lands and the run completes;
//! - a short explicit `host.call_timeout` ends the wait as a known failure;
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
    assert_ne!(
        view.state,
        SessionState::NeedsReconciliation,
        "a known stop must not require reconciliation"
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
async fn short_host_call_timeout_ends_the_wait_as_a_known_failure() {
    let mut config = RuntimeConfig {
        permissions: vec![ask_permission()],
        ..Default::default()
    };
    // The service stops at 3/4 of the Host deadline, before the Host's own
    // unknown-outcome timeout and far before CodeMode's 60-second host wait.
    config.host.call_timeout = Duration::from_millis(1500);
    let (_dir, rt) = setup(config);
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;

    let outcome = timeout(Duration::from_secs(15), task)
        .await
        .expect("the configured Host deadline must end the wait")
        .expect("the worker task must not panic");
    let _ = outcome;
    // The agent turns the known tool failure into its final answer.
    assert_eq!(
        assert_known_ended_wait(&rt, &id, &question.id),
        TaskState::Completed
    );
}

#[tokio::test]
async fn runtime_absolute_deadline_still_ends_a_paused_human_wait() {
    let (_dir, rt) = setup(RuntimeConfig {
        permissions: vec![ask_permission()],
        max_run_time: Duration::from_secs(4),
        ..Default::default()
    });
    let id = rt.create(None).unwrap().id;
    let task = start(&rt, &id);
    let question = pending(&rt, &id).await;

    // The default Host deadline is 60 seconds and the service stop is 45
    // seconds. Finishing now therefore proves the Runtime's absolute task
    // deadline still applies while the CodeMode compute clock is paused.
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
