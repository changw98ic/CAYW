//! Verification tests for RT-13:
//! Local agent fleet, stop, and steer routing through Host authorization,
//! CallScope lifecycle (started -> settled), Requests watch, and caller session fencing.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_runtime::{
    ApprovalChoice, LocalAgents, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskState,
};
use futures::stream;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const WATCHDOG: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ReviewFixture {
    requests: Mutex<Vec<ModelRequest>>,
}

impl Provider for ReviewFixture {
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

        if prompt == "hello" {
            let mut response = AssistantMessage::new(&request.model);
            response.stop_reason = StopReason::Stop;
            response.content.push(Content::text("child received steer"));
            response.usage = cayw_ai::Usage {
                input: 4,
                output: 4,
                total_tokens: 8,
                ..Default::default()
            };
            response.usage_status = cayw_ai::UsageStatus::Reported;
            self.requests.lock().unwrap().push(request);
            return Box::pin(stream::iter([AssistantEvent::Done { message: response }]));
        }

        let script: String = match prompt.as_str() {
            "bg-child-loop" | "bg-child-steerable" => {
                "return cayw.call('user.ask', {question='Hold child at tool boundary',options={'release'}})".into()
            }
            "spawn bg" => {
                "return cayw.call('agent.run', {prompt='bg-child-loop', label='BgChild', mode='background'})".into()
            }
            "spawn steerable bg" => {
                "return cayw.call('agent.run', {prompt='bg-child-steerable', label='SteerableChild', mode='background'})".into()
            }
            "call fleet" => "return cayw.call('agent.fleet', {})".into(),
            "call stop self" => {
                "local view = cayw.call('agent.fleet', {})\nreturn cayw.call('agent.stop', {child_task_id=view.members[1].child_task_id})".into()
            }
            other if other.starts_with("call fleet target ") => {
                let target = other.trim_start_matches("call fleet target ");
                format!("return cayw.call('agent.fleet', {{parent_session_id='{target}'}})")
            }
            other if other.starts_with("call stop target ") => {
                let target = other.trim_start_matches("call stop target ");
                format!("return cayw.call('agent.stop', {{child_task_id='{target}'}})")
            }
            other if other.starts_with("call steer target ") => {
                let target = other.trim_start_matches("call steer target ");
                format!("return cayw.call('agent.steer', {{child_task_id='{target}', text='hello'}})")
            }
            other => panic!("unexpected fixture prompt: {other}"),
        };

        let mut response = cayw_runtime::fixture::script_response(
            &request,
            &script,
            &format!("fixture-{}", request.messages.len()),
        );
        response.usage.total_tokens = 8;
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([AssistantEvent::Done { message: response }]))
    }
}

fn open_runtime(
    temp: &tempfile::TempDir,
    provider: Arc<ReviewFixture>,
    permissions: Vec<ScopedPermission>,
) -> Runtime {
    std::fs::create_dir_all(temp.path().join("workspace")).unwrap();
    Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "local-agents"),
        RuntimeConfig {
            max_concurrent_runs: 8,
            max_run_time: WATCHDOG,
            permissions,
            local_agents: Some(LocalAgents {
                max_children: 8,
                max_depth: 1,
                shared_total_tokens: 100_000,
                max_calls: 16,
                max_output_bytes: 64 * 1024,
                max_time: Duration::from_secs(10),
                permissions: vec![ScopedPermission::exact(
                    "user.ask",
                    json!({"question":"Hold child at tool boundary","options":["release"]}),
                )],
            }),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn run_task(
    runtime: &Runtime,
    session_id: &str,
    prompt: &str,
) -> Result<TaskState, RuntimeError> {
    let admitted = runtime.enqueue(session_id, prompt)?;
    let rt = runtime.clone();
    let task_id = admitted.id.clone();
    let resume_id = task_id.clone();
    let handle = tokio::spawn(async move { rt.resume(&resume_id, CancellationToken::new()).await });
    let result = tokio::time::timeout(WATCHDOG, handle)
        .await
        .expect("prompt timed out")
        .expect("worker must not panic");
    let task = runtime.task(&task_id)?;
    if result.is_err() || task.state != TaskState::Completed {
        eprintln!("prompt {prompt:?}: task={task:?}");
        if let Err(error) = result {
            eprintln!("runtime error: {error}");
        }
    }
    Ok(task.state)
}

async fn approve_next(rt: &Runtime, session_id: &str) {
    let sid = session_id.to_string();
    let rt = rt.clone();
    tokio::time::timeout(WATCHDOG, async move {
        loop {
            if let Some(a) = rt.get(&sid).unwrap().pending_approvals.first() {
                rt.approve(&sid, &a.id, ApprovalChoice::AllowOnce, "test-admin")
                    .expect("approval decision succeeded");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approval was requested");
}

async fn wait_child_question(runtime: &Runtime, session_id: &str) -> cayw_runtime::QuestionView {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(question) = runtime
                .pending_questions(session_id)
                .unwrap()
                .into_iter()
                .next()
            {
                assert_eq!(question.question, "Hold child at tool boundary");
                assert!(!runtime.model_requests(session_id).unwrap().budget_unknown);
                return question;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child must reach its authorized tool after model settlement")
}

#[tokio::test]
async fn test_fleet_denied_without_host_permission() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(ReviewFixture::default());
    // Permission set contains NO permission for agent.fleet
    let permissions = vec![];
    let runtime = open_runtime(&temp, provider, permissions);

    let session = runtime.create(Some("unauthorized-fleet".into())).unwrap();
    let rt = runtime.clone();
    let sid = session.id.clone();
    let denier = tokio::spawn(async move {
        let app = tokio::time::timeout(WATCHDOG, async {
            loop {
                if let Some(a) = rt.get(&sid).unwrap().pending_approvals.first() {
                    return a.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approval was requested");
        rt.approve(&sid, &app.id, ApprovalChoice::Deny, "test-admin")
            .expect("approval denied");
    });

    let state = run_task(&runtime, &session.id, "call fleet").await.unwrap();
    denier.await.unwrap();

    // The Luau script receives the gate denial as a known capability error,
    // so the agent completes its turn normally with TaskState::Completed.
    assert_eq!(state, TaskState::Completed);

    // Verify in Core events: capability was NOT started because gate rejected it
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    let has_started = events.iter().any(|r| {
        let val = serde_json::to_value(r).unwrap();
        val["event"]["type"] == "capability_started"
            && val["event"]["call"]["name"] == "agent.fleet"
    });
    assert!(
        !has_started,
        "agent.fleet must not start without Host authorization"
    );

    let settled_not_executed = events.iter().any(|r| {
        let val = serde_json::to_value(r).unwrap();
        val["event"]["type"] == "capability_settled"
            && val["event"]["call"]["name"] == "agent.fleet"
            && val["event"]["call"]["outcome"]["type"] == "not_executed"
    });
    assert!(
        settled_not_executed,
        "gate refusal must settle as not_executed"
    );
}

#[tokio::test]
async fn test_fleet_call_scope_started_and_settled_completed_when_authorized() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(ReviewFixture::default());
    // Grant explicit permission for agent.fleet with empty arguments {}
    let permissions = vec![ScopedPermission::exact("agent.fleet", json!({}))];
    let runtime = open_runtime(&temp, provider, permissions);

    let session = runtime.create(Some("authorized-fleet".into())).unwrap();
    let state = run_task(&runtime, &session.id, "call fleet").await.unwrap();
    assert_eq!(state, TaskState::Completed);

    // Verify in Core events:
    // 1. capability_started event exists with started: true
    // 2. capability_settled event exists with outcome: completed (NOT not_executed!)
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    let started = events.iter().find(|r| {
        let val = serde_json::to_value(r).unwrap();
        val["event"]["type"] == "capability_started"
            && val["event"]["call"]["name"] == "agent.fleet"
    });
    assert!(
        started.is_some(),
        "authorized agent.fleet must record capability_started via CallScope"
    );

    let settled = events.iter().find(|r| {
        let val = serde_json::to_value(r).unwrap();
        val["event"]["type"] == "capability_settled"
            && val["event"]["call"]["name"] == "agent.fleet"
    });
    assert!(
        settled.is_some(),
        "authorized agent.fleet must record capability_settled via CallScope"
    );
    let settled_val = serde_json::to_value(settled.unwrap()).unwrap();
    assert_eq!(
        settled_val["event"]["call"]["outcome"]["type"], "completed",
        "authorized agent.fleet outcome must be completed, not not_executed"
    );
}

#[tokio::test]
async fn test_cross_session_fleet_and_stop_authority_fencing() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(ReviewFixture::default());
    let permissions = vec![
        ScopedPermission::exact(
            "agent.run",
            json!({"label": "BgChild", "mode": "background", "prompt": "bg-child-loop"}),
        ),
        ScopedPermission::exact("agent.fleet", json!({})),
    ];
    let runtime = open_runtime(&temp, provider, permissions);

    let session_a = runtime.create(Some("session-a".into())).unwrap();
    let session_b = runtime.create(Some("session-b".into())).unwrap();

    // Session A spawns a background child
    let state_a = run_task(&runtime, &session_a.id, "spawn bg").await.unwrap();
    assert_eq!(state_a, TaskState::Completed);

    let fleet_a = runtime.fleet(Some(&session_a.id), None, 10).unwrap();
    assert_eq!(fleet_a.members.len(), 1);
    let child_task_id = fleet_a.members[0].child_task_id.clone();
    let child_session_id = fleet_a.members[0].child_session_id.clone();
    let _question = wait_child_question(&runtime, &child_session_id).await;
    let child_before = runtime.get(&child_session_id).unwrap();

    // 1. Session B attempts to query Session A via agent.fleet -> rejected by fleet service
    let decider = tokio::spawn({
        let rt = runtime.clone();
        let sid = session_b.id.clone();
        async move {
            approve_next(&rt, &sid).await;
        }
    });
    let fleet_prompt = format!("call fleet target {}", session_a.id);
    let state_b_fleet = run_task(&runtime, &session_b.id, &fleet_prompt)
        .await
        .unwrap();
    decider.await.unwrap();
    assert_eq!(
        state_b_fleet,
        TaskState::Completed,
        "querying another session via agent.fleet completes turn with tool error"
    );

    // 2. Session B attempts to stop Session A's child via agent.stop -> rejected by fleet service
    let stop_prompt = format!("call stop target {child_task_id}");
    let decider = tokio::spawn({
        let rt = runtime.clone();
        let sid = session_b.id.clone();
        async move {
            approve_next(&rt, &sid).await;
        }
    });
    let state_b_stop = run_task(&runtime, &session_b.id, &stop_prompt)
        .await
        .unwrap();
    decider.await.unwrap();
    assert_eq!(
        state_b_stop,
        TaskState::Completed,
        "stopping another session's child task completes turn with tool error"
    );

    // 3. Session B attempts to steer Session A's child via agent.steer -> rejected by fleet service
    let steer_prompt = format!("call steer target {child_task_id}");
    let decider = tokio::spawn({
        let rt = runtime.clone();
        let sid = session_b.id.clone();
        async move {
            approve_next(&rt, &sid).await;
        }
    });
    let state_b_steer = run_task(&runtime, &session_b.id, &steer_prompt)
        .await
        .unwrap();
    decider.await.unwrap();
    assert_eq!(
        state_b_steer,
        TaskState::Completed,
        "steering another session's child task completes turn with tool error"
    );

    let events_b: Vec<_> = runtime
        .events(&session_b.id, 0, 1000)
        .unwrap()
        .iter()
        .map(|record| serde_json::to_value(record).unwrap())
        .collect();
    for (name, expected_error) in [
        (
            "agent.fleet",
            format!(
                "caller session {} is not authorized to query session {}",
                session_b.id, session_a.id
            ),
        ),
        (
            "agent.stop",
            format!(
                "caller session {} is not authorized to control child task {child_task_id}",
                session_b.id
            ),
        ),
        (
            "agent.steer",
            format!(
                "caller session {} is not authorized to control child task {child_task_id}",
                session_b.id
            ),
        ),
    ] {
        let settled = events_b
            .iter()
            .find(|record| {
                record["event"]["type"] == "capability_settled"
                    && record["event"]["call"]["name"] == name
            })
            .expect("foreign capability must have a terminal record");
        let call = &settled["event"]["call"];
        assert_eq!(call["outcome"]["type"], "failed", "{name}: {call}");
        assert!(
            call["outcome"]["error"]
                .as_str()
                .unwrap()
                .contains(&expected_error),
            "{name}: {call}"
        );
        assert!(
            !call["key"].is_null(),
            "capability key must be present: {call}"
        );
        assert!(
            events_b.iter().any(|record| {
                record["event"]["type"] == "capability_started"
                    && record["event"]["call"]["key"] == call["key"]
                    && record["event"]["call"]["name"] == name
            }),
            "{name} must start after Host authorization before the service rejects it"
        );
    }
    assert_eq!(
        serde_json::to_value(runtime.fleet(Some(&session_a.id), None, 10).unwrap()).unwrap(),
        serde_json::to_value(&fleet_a).unwrap()
    );
    assert_eq!(runtime.get(&child_session_id).unwrap(), child_before);

    // 4. Session A stops its OWN child via agent.stop -> must succeed
    let decider = tokio::spawn({
        let rt = runtime.clone();
        let sid = session_a.id.clone();
        async move {
            approve_next(&rt, &sid).await;
        }
    });
    let state_a_stop = run_task(&runtime, &session_a.id, "call stop self")
        .await
        .unwrap();
    decider.await.unwrap();
    assert_eq!(
        state_a_stop,
        TaskState::Completed,
        "session A stopping its own child must succeed"
    );

    // Verify Session A's agent.stop settled with completed outcome
    let events_a = runtime.events(&session_a.id, 0, 1000).unwrap();
    let stop_settled = events_a.iter().find(|r| {
        let val = serde_json::to_value(r).unwrap();
        val["event"]["type"] == "capability_settled" && val["event"]["call"]["name"] == "agent.stop"
    });
    assert!(stop_settled.is_some());
    let stop_val = serde_json::to_value(stop_settled.unwrap()).unwrap();
    assert_eq!(stop_val["event"]["call"]["outcome"]["type"], "completed");
}

#[tokio::test]
async fn test_child_receives_and_consumes_steered_message_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(ReviewFixture::default());
    let permissions = vec![
        ScopedPermission::exact(
            "agent.run",
            json!({"label": "SteerableChild", "mode": "background", "prompt": "bg-child-steerable"}),
        ),
        ScopedPermission::exact("agent.fleet", json!({})),
    ];
    let runtime = open_runtime(&temp, provider.clone(), permissions);

    let session = runtime.create(Some("parent-steer".into())).unwrap();

    // 1. Parent spawns a background steerable child
    let spawn_state = run_task(&runtime, &session.id, "spawn steerable bg")
        .await
        .unwrap();
    assert_eq!(spawn_state, TaskState::Completed);

    let fleet = runtime.fleet(Some(&session.id), None, 10).unwrap();
    assert_eq!(fleet.members.len(), 1);
    let child_task_id = fleet.members[0].child_task_id.clone();

    // 2. The child has completed its model request and waits in an authorized tool.
    let question = wait_child_question(&runtime, &fleet.members[0].child_session_id).await;

    // 3. Parent calls agent.steer via Luau cell
    let steer_prompt = format!("call steer target {child_task_id}");
    let decider = tokio::spawn({
        let rt = runtime.clone();
        let sid = session.id.clone();
        async move {
            approve_next(&rt, &sid).await;
        }
    });
    let steer_state = run_task(&runtime, &session.id, &steer_prompt)
        .await
        .unwrap();
    decider.await.unwrap();
    assert_eq!(steer_state, TaskState::Completed);

    // 4. Release the original tool so the next model request consumes the steer.
    runtime
        .answer_question(&question.session_id, &question.id, "release", "test-admin")
        .unwrap();

    // 5. Wait for child to complete
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let task = runtime.task(&child_task_id).unwrap();
            if task.state == TaskState::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("child completion timed out");

    // 6. Verify child requests: turn 2 request must contain the steered message "hello"
    let requests = provider.requests.lock().unwrap();
    let child_requests: Vec<_> = requests
        .iter()
        .filter(|req| {
            req.messages.iter().any(|m| match m {
                Message::User { .. } => m.text() == "bg-child-steerable",
                _ => false,
            })
        })
        .collect();

    assert!(
        child_requests.len() >= 2,
        "child must have at least 2 requests (turn 1 + turn 2 after steering)"
    );
    let turn2_req = child_requests.last().unwrap();
    let last_user_msg = turn2_req
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { .. } => Some(m.text()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        last_user_msg, "hello",
        "turn 2 request must have received the steered user message"
    );

    // 7. Verify Core events for parent: agent.steer had started and settled completed
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    let steer_settled = events.iter().find(|r| {
        let val = serde_json::to_value(r).unwrap();
        val["event"]["type"] == "capability_settled"
            && val["event"]["call"]["name"] == "agent.steer"
    });
    assert!(
        steer_settled.is_some(),
        "parent session must record agent.steer capability_settled"
    );
    let val = serde_json::to_value(steer_settled.unwrap()).unwrap();
    assert_eq!(val["event"]["call"]["outcome"]["type"], "completed");
}
