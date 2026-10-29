//! Verification tests for RT-12: FleetView bounded pagination, limits, and member controls.

use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_runtime::{
    FleetMemberState, LocalAgents, Runtime, RuntimeConfig, RuntimeError, ScopedPermission,
    TaskState,
};
use futures::stream;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const WATCHDOG: Duration = Duration::from_secs(10);

#[derive(Default)]
struct FleetFixture {
    requests: Mutex<Vec<ModelRequest>>,
}

impl Provider for FleetFixture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let prompt = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { .. } => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default();

        let script = match prompt.as_str() {
            prompt if prompt.starts_with("bg-task-") => {
                "return cayw.call('user.ask', {question='Hold fleet child at tool boundary',options={'release'}})"
            }
            "spawn bg 1" => {
                "return cayw.call('agent.run', {prompt='bg-task-1', label='Bg-1', mode='background'})"
            }
            "spawn bg 2" => {
                "return cayw.call('agent.run', {prompt='bg-task-2', label='Bg-2', mode='background'})"
            }
            "spawn bg 3" => {
                "return cayw.call('agent.run', {prompt='bg-task-3', label='Bg-3', mode='background'})"
            }
            other => panic!("unexpected fixture prompt: {other}"),
        };

        let mut response = cayw_runtime::fixture::script_response(
            &request,
            script,
            &format!("fixture-{}", request.messages.len()),
        );
        response.usage.total_tokens = 8;
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([AssistantEvent::Done { message: response }]))
    }
}

fn create_runtime(temp: &tempfile::TempDir, provider: Arc<FleetFixture>) -> Runtime {
    std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
    Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "local-agents"),
        RuntimeConfig {
            max_concurrent_runs: 8,
            max_run_time: WATCHDOG,
            permissions: vec![
                ScopedPermission::exact(
                    "agent.run",
                    json!({"label": "Bg-1", "mode": "background", "prompt": "bg-task-1"}),
                ),
                ScopedPermission::exact(
                    "agent.run",
                    json!({"label": "Bg-2", "mode": "background", "prompt": "bg-task-2"}),
                ),
                ScopedPermission::exact(
                    "agent.run",
                    json!({"label": "Bg-3", "mode": "background", "prompt": "bg-task-3"}),
                ),
            ],
            local_agents: Some(LocalAgents {
                max_children: 8,
                max_depth: 1,
                shared_total_tokens: 100_000,
                max_calls: 16,
                max_output_bytes: 64 * 1024,
                max_time: Duration::from_secs(10),
                permissions: vec![ScopedPermission::exact(
                    "user.ask",
                    json!({"question": "Hold fleet child at tool boundary", "options": ["release"]}),
                )],
            }),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn run_prompt(
    runtime: &Runtime,
    session_id: &str,
    prompt: &str,
) -> cayw_runtime::QuestionView {
    let before = runtime.fleet(Some(session_id), None, 10).unwrap();
    let admitted = runtime.enqueue(session_id, prompt).unwrap();
    let rt = runtime.clone();
    let task_id = admitted.id.clone();
    let resume_id = task_id.clone();
    let handle = tokio::spawn(async move { rt.resume(&resume_id, CancellationToken::new()).await });
    let view = tokio::time::timeout(WATCHDOG, handle)
        .await
        .expect("prompt timed out")
        .expect("worker must not panic")
        .expect("task failed");
    assert!(view.messages.len() > 1);
    assert_eq!(runtime.task(&task_id).unwrap().state, TaskState::Completed);

    let after = runtime.fleet(Some(session_id), None, 10).unwrap();
    assert_eq!(after.members.len(), before.members.len() + 1);
    let child = after
        .members
        .iter()
        .find(|member| {
            !before
                .members
                .iter()
                .any(|old| old.child_task_id == member.child_task_id)
        })
        .unwrap();
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(question) = runtime
                .pending_questions(&child.child_session_id)
                .unwrap()
                .into_iter()
                .next()
            {
                assert_eq!(question.question, "Hold fleet child at tool boundary");
                assert_eq!(
                    runtime.task(&child.child_task_id).unwrap().state,
                    TaskState::Running
                );
                assert_eq!(child.state, FleetMemberState::Running);
                assert!(
                    !runtime
                        .model_requests(&child.child_session_id)
                        .unwrap()
                        .budget_unknown
                );
                return question;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child must reach its authorized tool after model settlement")
}

async fn wait_child_terminal(
    runtime: &Runtime,
    question: &cayw_runtime::QuestionView,
    task_state: TaskState,
    fleet_state: FleetMemberState,
) {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let task = runtime.task(&question.task_id).unwrap();
            let member = runtime
                .fleet(None, None, 10)
                .unwrap()
                .members
                .into_iter()
                .find(|member| member.child_task_id == question.task_id)
                .unwrap();
            if task.state == task_state && member.state == fleet_state {
                assert!(
                    task.error.is_none(),
                    "unexpected child error: {:?}",
                    task.error
                );
                assert!(runtime.get(&question.session_id).unwrap().attempt.is_none());
                assert!(
                    runtime
                        .pending_questions(&question.session_id)
                        .unwrap()
                        .is_empty()
                );
                return;
            }
            assert!(
                matches!(task.state, TaskState::Running | TaskState::Draining)
                    || task.state == task_state,
                "unexpected child terminal: task={}, state={:?}, error={:?}, fleet={:?}",
                task.id,
                task.state,
                task.error,
                member.state
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child task and fleet must reach the requested terminal");
}

async fn complete_child(runtime: &Runtime, question: &cayw_runtime::QuestionView) {
    runtime
        .answer_question(&question.session_id, &question.id, "release", "test-admin")
        .unwrap();
    wait_child_terminal(
        runtime,
        question,
        TaskState::Completed,
        FleetMemberState::Completed,
    )
    .await;
}

async fn stop_child(runtime: &Runtime, question: &cayw_runtime::QuestionView) {
    assert_eq!(
        runtime.task(&question.task_id).unwrap().state,
        TaskState::Running
    );
    let stop = runtime.fleet_stop(&question.task_id).unwrap();
    assert_eq!(stop["stopped"], true);
    wait_child_terminal(
        runtime,
        question,
        TaskState::Cancelled,
        FleetMemberState::Cancelled,
    )
    .await;
    let repeated = runtime.fleet_stop(&question.task_id).unwrap();
    assert_eq!(repeated["stopped"], false);
    assert_eq!(repeated["reason"], "not_running");
}

#[tokio::test]
async fn test_fleet_limit_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(FleetFixture::default());
    let runtime = create_runtime(&temp, provider);

    // limit = 0 is rejected
    assert!(matches!(
        runtime.fleet(None, None, 0),
        Err(RuntimeError::InvalidInput(_))
    ));

    // limit = 1001 is rejected
    assert!(matches!(
        runtime.fleet(None, None, 1001),
        Err(RuntimeError::InvalidInput(_))
    ));

    // Valid limits
    let view_1 = runtime.fleet(None, None, 1).unwrap();
    assert!(view_1.members.is_empty());
    assert!(view_1.next_after.is_none());

    let view_1000 = runtime.fleet(None, None, 1000).unwrap();
    assert!(view_1000.members.is_empty());
    assert!(view_1000.next_after.is_none());
}

#[tokio::test]
async fn test_fleet_range_pagination_limit_one() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(FleetFixture::default());
    let runtime = create_runtime(&temp, provider);

    let parent = runtime.create(Some("parent".into())).unwrap();

    // user.ask holds the workspace mutation fence until its answer settles.
    // Completed children remain in FleetView, so settle each before the next spawn.
    for prompt in ["spawn bg 1", "spawn bg 2", "spawn bg 3"] {
        let question = run_prompt(&runtime, &parent.id, prompt).await;
        complete_child(&runtime, &question).await;
    }

    // Snapshot with limit = 1 should fetch only 1 item and return next_after
    let page1 = runtime.fleet(Some(&parent.id), None, 1).unwrap();
    assert_eq!(page1.members.len(), 1);
    assert!(page1.next_after.is_some());
    let cursor1 = page1.next_after.as_deref().unwrap();
    assert_eq!(page1.members[0].child_task_id, cursor1);

    // Fetch page 2 using cursor1
    let page2 = runtime.fleet(Some(&parent.id), Some(cursor1), 1).unwrap();
    assert_eq!(page2.members.len(), 1);
    assert!(page2.next_after.is_some());
    let cursor2 = page2.next_after.as_deref().unwrap();
    assert_eq!(page2.members[0].child_task_id, cursor2);
    assert!(cursor2 > cursor1);

    // Fetch page 3 using cursor2
    let page3 = runtime.fleet(Some(&parent.id), Some(cursor2), 1).unwrap();
    assert_eq!(page3.members.len(), 1);
    assert!(page3.next_after.is_none()); // End of pages
    assert!(page3.members[0].child_task_id.as_str() > cursor2);

    // Fetch page 4 using last task id -> should be empty
    let page4 = runtime
        .fleet(Some(&parent.id), Some(&page3.members[0].child_task_id), 1)
        .unwrap();
    assert!(page4.members.is_empty());
    assert!(page4.next_after.is_none());

    // Verify all 3 match the full snapshot
    let full = runtime.fleet(Some(&parent.id), None, 10).unwrap();
    assert_eq!(full.members.len(), 3);
    assert_eq!(
        full.members[0].child_task_id,
        page1.members[0].child_task_id
    );
    assert_eq!(
        full.members[1].child_task_id,
        page2.members[0].child_task_id
    );
    assert_eq!(
        full.members[2].child_task_id,
        page3.members[0].child_task_id
    );

    // Every original child reached a known terminal before it entered this page.
    for member in &full.members {
        assert_eq!(member.state, FleetMemberState::Completed);
        assert_eq!(
            runtime.task(&member.child_task_id).unwrap().state,
            TaskState::Completed
        );
        let stop = runtime.fleet_stop(&member.child_task_id).unwrap();
        assert_eq!(stop["stopped"], false);
        assert_eq!(stop["reason"], "not_running");
    }
}

#[tokio::test]
async fn test_fleet_parent_scoping_and_trusted_sdk_stop() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(FleetFixture::default());
    let runtime = create_runtime(&temp, provider);

    let parent_a = runtime.create(Some("parent-a".into())).unwrap();
    let parent_b = runtime.create(Some("parent-b".into())).unwrap();

    let child_a = run_prompt(&runtime, &parent_a.id, "spawn bg 1").await;
    stop_child(&runtime, &child_a).await;
    let child_b = run_prompt(&runtime, &parent_b.id, "spawn bg 2").await;

    let view_a = runtime.fleet(Some(&parent_a.id), None, 10).unwrap();
    let view_b = runtime.fleet(Some(&parent_b.id), None, 10).unwrap();
    let view_all = runtime.fleet(None, None, 10).unwrap();

    assert_eq!(view_a.members.len(), 1);
    assert_eq!(view_a.members[0].parent_session_id, parent_a.id);

    assert_eq!(view_b.members.len(), 1);
    assert_eq!(view_b.members[0].parent_session_id, parent_b.id);

    assert_eq!(view_all.members.len(), 2);

    assert_eq!(view_a.members[0].child_task_id, child_a.task_id);
    assert_eq!(view_a.members[0].state, FleetMemberState::Cancelled);
    assert_eq!(view_b.members[0].child_task_id, child_b.task_id);
    assert_eq!(view_b.members[0].state, FleetMemberState::Running);

    // Trusted SDK stop reaches the original running child and waits for its cleanup.
    stop_child(&runtime, &child_b).await;
}
