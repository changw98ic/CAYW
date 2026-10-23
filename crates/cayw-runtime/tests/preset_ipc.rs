//! Issue 5 preset capability RPC chain over a real authenticated `cayw-ipc`
//! TCP connection, not a direct `RpcHandler` call. One test drives a queued
//! image task through `plan.update` and `user.ask` to an authenticated answer.
//! The other tests prove unauthenticated transports cannot answer or enqueue,
//! and that cancellation or disconnection leaves a known, question-expired
//! durable state rather than an unknown side effect.

use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Content, Message, Model, ModelRequest,
    Provider,
};
use cayw_core::{CoreEvent, EventRecord};
use cayw_ipc::{Client, PROTOCOL_VERSION, RpcHandler, ServerConfig, serve};
use cayw_runtime::{
    PlanStepStatus, QuestionState, Runtime, RuntimeConfig, ScopedPermission, TaskState,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

const TOKEN: &str = "preset-ipc-token";
/// Canonical standard padded base64 of a 1x1 PNG. The runtime re-encodes and
/// compares it, so a non-canonical fixture would fail before any task row exists.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
const ASK_ONLY: &str = r#"return cayw.call('user.ask',{question='Which target should I use?',options={'staging','production'}})"#;
const PLAN_THEN_ASK: &str = r#"cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect workspace',status='in_progress'},{id='edit',text='Apply edit',status='pending'}}})
return cayw.call('user.ask',{question='Which target should I use?',options={'staging','production'}})"#;

struct Recording {
    script: &'static str,
    seen: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for Recording {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.seen.lock().unwrap().push(request.clone());
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.script,
            &format!("tool-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn setup(
    script: &'static str,
    supports_images: bool,
    permissions: Vec<ScopedPermission>,
) -> (tempfile::TempDir, Runtime, Arc<Mutex<Vec<ModelRequest>>>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Recording {
            script,
            seen: seen.clone(),
        }),
        Model {
            supports_images,
            ..Model::new("fixture", "preset-ipc")
        },
        RuntimeConfig {
            permissions,
            ..Default::default()
        },
    )
    .unwrap();
    (dir, runtime, seen)
}

async fn server(
    runtime: Runtime,
) -> (
    std::net::SocketAddr,
    CancellationToken,
    JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let task = tokio::spawn(async move {
        serve(
            listener,
            Arc::new(runtime) as Arc<dyn RpcHandler>,
            ServerConfig::new(TOKEN),
            signal,
        )
        .await
    });
    (address, stop, task)
}

async fn call(client: &Client, method: &str, params: Value) -> Value {
    client
        .request(method, params)
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error}"))
}

async fn wait_pending(client: &Client, session: &str) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let pending = call(
                client,
                "session.pending_questions",
                json!({"session_id":session}),
            )
            .await;
            if let Some(question) = pending.as_array().and_then(|list| list.first()) {
                return question.clone();
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("question must become pending over authenticated IPC")
}

async fn wait_question_settled(client: &Client, session: &str) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let questions = call(client, "session.questions", json!({"session_id":session})).await;
            let question = questions
                .as_array()
                .and_then(|list| list.first())
                .cloned()
                .expect("the asked question must stay durable");
            if question["state"] != "pending" {
                return question;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled question must leave the pending state")
}

async fn wait_task_settled(client: &Client, session: &str) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let tasks = call(client, "session.tasks", json!({"session_id":session})).await;
            let state = tasks
                .as_array()
                .and_then(|list| list.first())
                .and_then(|task| task["state"].as_str())
                .expect("the enqueued task must stay durable");
            if !matches!(state, "queued" | "running" | "draining") {
                return tasks;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runtime must settle the cancelled task")
}

fn host_kinds(events: &Value) -> Vec<String> {
    let records: Vec<EventRecord> =
        serde_json::from_value(events.clone()).expect("session.events returns Core records");
    records
        .into_iter()
        .filter_map(|record| match record.event {
            CoreEvent::Host { kind, .. } => Some(kind),
            _ => None,
        })
        .collect()
}

fn count(kinds: &[String], kind: &str) -> usize {
    kinds
        .iter()
        .filter(|actual| actual.as_str() == kind)
        .count()
}

fn ask_scope() -> Value {
    json!({"question":"Which target should I use?","options":["staging","production"]})
}

fn plan_scope() -> Value {
    json!({
        "expected_version":0,
        "steps":[
            {"id":"inspect","text":"Inspect workspace","status":"in_progress"},
            {"id":"edit","text":"Apply edit","status":"pending"}
        ]
    })
}

async fn enqueue_ask_task(client: &Client, session: &str) -> String {
    let queued = call(
        client,
        "task.enqueue",
        json!({"session_id":session,"text":"ask the operator"}),
    )
    .await;
    assert_eq!(queued["state"], "queued");
    queued["id"].as_str().unwrap().to_string()
}

async fn create_session(client: &Client, label: &str) -> String {
    let created = call(client, "session.create", json!({"label":label})).await;
    assert_eq!(created["label"], label);
    created["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn authenticated_rpc_round_trip_carries_image_plan_question_and_answer() {
    let (_dir, runtime, seen) = setup(
        PLAN_THEN_ASK,
        true,
        vec![
            ScopedPermission::exact("plan.update", plan_scope()),
            ScopedPermission::exact("user.ask", ask_scope()),
        ],
    );
    let (address, stop, server_task) = server(runtime.clone()).await;
    let client = Client::connect(address, TOKEN).await.unwrap();

    let health = call(&client, "runtime.health", json!({})).await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["provider"], "fixture");
    assert_eq!(health["model"], "preset-ipc");

    let id = create_session(&client, "preset rpc").await;
    let enqueued = call(
        &client,
        "task.enqueue",
        json!({
            "session_id":id,
            "text":"Inspect the uploaded screenshot and propose a plan",
            "images":[{"mime_type":"image/png","data":PNG}],
        }),
    )
    .await;
    let task_id = enqueued["id"].as_str().unwrap().to_string();
    assert_eq!(enqueued["state"], "queued");
    let fetched = call(&client, "task.get", json!({"task_id":task_id})).await;
    assert_eq!(fetched["id"].as_str(), Some(task_id.as_str()));
    assert_eq!(fetched["state"], "queued");
    assert_eq!(
        fetched["input"]["text"],
        "Inspect the uploaded screenshot and propose a plan"
    );
    assert_eq!(
        fetched["input"]["images"][0],
        json!({"mime_type":"image/png","data":PNG})
    );

    let runner = client.clone();
    let run_id = task_id.clone();
    let running = tokio::spawn(async move {
        runner
            .request("task.resume", json!({"task_id":run_id}))
            .await
    });

    let question = wait_pending(&client, &id).await;
    let question_id = question["id"].as_str().unwrap().to_string();
    assert_eq!(question["session_id"].as_str(), Some(id.as_str()));
    assert_eq!(question["task_id"].as_str(), Some(task_id.as_str()));
    assert_eq!(question["question"], "Which target should I use?");
    assert_eq!(question["options"], json!(["staging", "production"]));
    assert_eq!(question["state"], "pending");
    assert!(question["answer"].is_null());

    let listed = call(&client, "session.questions", json!({"session_id":id})).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"].as_str(), Some(question_id.as_str()));
    assert_eq!(listed[0]["state"], "pending");

    let plan = call(&client, "session.plan", json!({"session_id":id})).await;
    assert_eq!(plan["version"], 1);
    assert_eq!(plan["previous_version"], 0);
    assert_eq!(plan["source"], "model");
    assert_eq!(plan["session_id"].as_str(), Some(id.as_str()));
    assert_eq!(plan["task_id"].as_str(), Some(task_id.as_str()));
    assert_eq!(plan["steps"][0]["id"], "inspect");
    assert_eq!(plan["steps"][0]["status"], "in_progress");
    assert_eq!(plan["steps"][1]["id"], "edit");
    assert_eq!(plan["steps"][1]["status"], "pending");

    let events = call(
        &client,
        "session.events",
        json!({"session_id":id,"limit":1000}),
    )
    .await;
    let kinds = host_kinds(&events);
    assert_eq!(count(&kinds, "collaboration_plan_updated"), 1);
    assert_eq!(count(&kinds, "collaboration_question_asked"), 1);
    assert_eq!(count(&kinds, "collaboration_question_answered"), 0);

    let answered = call(
        &client,
        "question.answer",
        json!({"session_id":id,"request_id":question_id,"answer":"production"}),
    )
    .await;
    assert_eq!(answered["id"].as_str(), Some(question_id.as_str()));
    assert_eq!(answered["state"], "answered");
    assert_eq!(answered["answer"]["answer"], "production");
    assert_eq!(answered["answer"]["actor"], "rpc-user");

    let terminal = timeout(Duration::from_secs(15), running)
        .await
        .expect("the original run must finish after the RPC answer")
        .unwrap()
        .unwrap();
    assert_eq!(terminal["state"], "idle");
    assert!(terminal["attempt"].is_null());

    let tasks = call(&client, "session.tasks", json!({"session_id":id})).await;
    assert_eq!(tasks[0]["id"].as_str(), Some(task_id.as_str()));
    assert_eq!(tasks[0]["state"], "completed");
    assert_eq!(
        call(
            &client,
            "session.pending_questions",
            json!({"session_id":id})
        )
        .await,
        json!([])
    );
    let listed = call(&client, "session.questions", json!({"session_id":id})).await;
    assert_eq!(listed[0]["state"], "answered");
    let events = call(
        &client,
        "session.events",
        json!({"session_id":id,"limit":1000}),
    )
    .await;
    assert_eq!(
        count(&host_kinds(&events), "collaboration_question_answered"),
        1
    );

    let late = client
        .request(
            "question.answer",
            json!({"session_id":id,"request_id":question_id,"answer":"staging"}),
        )
        .await
        .unwrap_err();
    assert_eq!(late.code, "stale_approval");
    assert!(!late.outcome_unknown);
    // Answering a question is not a permission decision and creates no grant.
    assert_eq!(
        call(&client, "session.grants", json!({"session_id":id})).await,
        json!([])
    );

    // The same Core facts are visible through the local SDK handle.
    assert_eq!(
        runtime.questions(&id).unwrap()[0].state,
        QuestionState::Answered
    );
    assert_eq!(
        runtime.plan(&id).unwrap().unwrap().steps[0].status,
        PlanStepStatus::InProgress
    );
    assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert!(runtime.grants(&id).unwrap().is_empty());

    // The queued image crossed the IPC boundary unchanged and reached the provider.
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty());
    assert!(
        seen.iter()
            .any(|request| request.messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User { content, .. } if content.iter().any(|part| matches!(
                        part,
                        Content::Image { data, mime_type }
                            if data == PNG && mime_type == "image/png"
                    ))
                )
            })),
        "the provider must receive the exact image enqueued over IPC"
    );
    assert!(
        seen.iter()
            .any(|request| request.messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User { content, .. } if content.iter().any(|part| matches!(
                        part,
                        Content::Text { text } if text.contains("uploaded screenshot")
                    ))
                )
            }))
    );

    client.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn unauthenticated_transports_cannot_answer_or_touch_durable_facts() {
    let (_dir, runtime, _seen) = setup(
        ASK_ONLY,
        false,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let (address, stop, server_task) = server(runtime.clone()).await;
    let owner = Client::connect(address, TOKEN).await.unwrap();
    let id = create_session(&owner, "unauthenticated").await;
    let task_id = enqueue_ask_task(&owner, &id).await;

    let runner = owner.clone();
    let run_id = task_id.clone();
    let running = tokio::spawn(async move {
        runner
            .request("task.resume", json!({"task_id":run_id}))
            .await
    });
    let question = wait_pending(&owner, &id).await;
    let question_id = question["id"].as_str().unwrap().to_string();
    let questions_before = call(&owner, "session.questions", json!({"session_id":id})).await;
    let tasks_before = call(&owner, "session.tasks", json!({"session_id":id})).await;
    assert_eq!(questions_before[0]["state"], "pending");
    assert_eq!(tasks_before[0]["state"], "running");

    // A wrong token is rejected during hello and no business RPC is dispatched.
    let rejected = Client::connect(address, "not-the-token").await.unwrap_err();
    assert_eq!(rejected.code, "unauthorized");
    assert!(!rejected.outcome_unknown);

    // A raw socket cannot smuggle `question.answer` in place of hello.
    let mut raw = TcpStream::connect(address).await.unwrap();
    let frame = json!({
        "version":PROTOCOL_VERSION,
        "id":"raw-answer",
        "method":"question.answer",
        "params":{"session_id":id,"request_id":question_id,"answer":"production"},
    });
    raw.write_all(&serde_json::to_vec(&frame).unwrap())
        .await
        .unwrap();
    raw.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(raw);
    let mut line = String::new();
    let read = timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("the handshake rejection must be answered")
        .unwrap();
    assert!(read > 0);
    let rejected: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(rejected["id"], "raw-answer");
    assert_eq!(rejected["error"]["code"], "handshake_required");
    let mut eof = String::new();
    let closed = timeout(Duration::from_secs(5), reader.read_line(&mut eof))
        .await
        .expect("a rejected connection must close")
        .unwrap();
    assert_eq!(closed, 0, "the server closes a rejected connection");

    // Neither rejection changed a durable question, task or answer event.
    assert_eq!(
        call(&owner, "session.questions", json!({"session_id":id})).await,
        questions_before
    );
    assert_eq!(
        call(&owner, "session.tasks", json!({"session_id":id})).await,
        tasks_before
    );
    let events = call(
        &owner,
        "session.events",
        json!({"session_id":id,"limit":1000}),
    )
    .await;
    let kinds = host_kinds(&events);
    assert_eq!(count(&kinds, "collaboration_question_asked"), 1);
    assert_eq!(count(&kinds, "collaboration_question_answered"), 0);
    assert_eq!(
        runtime.questions(&id).unwrap()[0].state,
        QuestionState::Pending
    );
    assert!(runtime.questions(&id).unwrap()[0].answer.is_none());

    // The authenticated host can still answer and finish the original run.
    let answered = call(
        &owner,
        "question.answer",
        json!({"session_id":id,"request_id":question_id,"answer":"production"}),
    )
    .await;
    assert_eq!(answered["state"], "answered");
    assert_eq!(answered["answer"]["actor"], "rpc-user");
    let terminal = timeout(Duration::from_secs(15), running)
        .await
        .expect("the authenticated answer must finish the run")
        .unwrap()
        .unwrap();
    assert_eq!(terminal["state"], "idle");
    assert_eq!(
        call(&owner, "session.tasks", json!({"session_id":id})).await[0]["state"],
        "completed"
    );

    owner.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelling_resume_over_ipc_expires_question_without_unknown_effect() {
    let (_dir, runtime, _seen) = setup(
        ASK_ONLY,
        false,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let (address, stop, server_task) = server(runtime.clone()).await;
    let client = Client::connect(address, TOKEN).await.unwrap();
    let id = create_session(&client, "cancelled over ipc").await;
    let task_id = enqueue_ask_task(&client, &id).await;

    let cancel = CancellationToken::new();
    let runner = client.clone();
    let run_id = task_id.clone();
    let run_cancel = cancel.clone();
    let running = tokio::spawn(async move {
        runner
            .request_with_cancel("task.resume", json!({"task_id":run_id}), run_cancel)
            .await
    });
    let question = wait_pending(&client, &id).await;
    let question_id = question["id"].as_str().unwrap().to_string();

    cancel.cancel();
    let cancelled = timeout(Duration::from_secs(15), running)
        .await
        .expect("cancellation must reach a terminal RPC response")
        .unwrap();
    let error = cancelled.unwrap_err();
    assert_eq!(error.code, "cancelled_outcome_unknown");
    assert!(
        error.outcome_unknown,
        "the transport must not claim a cancelled run had no effects"
    );

    let question = wait_question_settled(&client, &id).await;
    assert_eq!(question["state"], "expired");
    assert!(question["answer"].is_null());
    let tasks = wait_task_settled(&client, &id).await;
    assert_eq!(tasks[0]["state"], "cancelled");
    assert_ne!(tasks[0]["state"], "outcome_unknown");

    let events = call(
        &client,
        "session.events",
        json!({"session_id":id,"limit":1000}),
    )
    .await;
    let kinds = host_kinds(&events);
    assert_eq!(count(&kinds, "collaboration_question_asked"), 1);
    assert_eq!(count(&kinds, "collaboration_question_answered"), 0);

    let late = client
        .request(
            "question.answer",
            json!({"session_id":id,"request_id":question_id,"answer":"production"}),
        )
        .await
        .unwrap_err();
    assert_eq!(late.code, "stale_approval");
    assert!(!late.outcome_unknown);
    assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Cancelled);
    assert!(runtime.questions(&id).unwrap()[0].answer.is_none());

    client.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn disconnecting_the_owning_connection_expires_question_without_unknown_effect() {
    let (_dir, runtime, _seen) = setup(
        ASK_ONLY,
        false,
        vec![ScopedPermission::exact("user.ask", ask_scope())],
    );
    let (address, stop, server_task) = server(runtime.clone()).await;
    let owner = Client::connect(address, TOKEN).await.unwrap();
    let observer = Client::connect(address, TOKEN).await.unwrap();
    let id = create_session(&owner, "disconnected over ipc").await;
    let task_id = enqueue_ask_task(&owner, &id).await;

    let runner = owner.clone();
    let run_id = task_id.clone();
    let running = tokio::spawn(async move {
        runner
            .request("task.resume", json!({"task_id":run_id}))
            .await
    });
    let _question = wait_pending(&observer, &id).await;

    owner.close().await;
    let lost = timeout(Duration::from_secs(15), running)
        .await
        .expect("closing the connection must end its pending request")
        .unwrap();
    assert!(
        lost.is_err(),
        "a closed connection cannot report a terminal success"
    );

    let question = wait_question_settled(&observer, &id).await;
    assert_eq!(question["state"], "expired");
    assert!(question["answer"].is_null());
    let tasks = wait_task_settled(&observer, &id).await;
    assert_eq!(tasks[0]["state"], "cancelled");
    assert_ne!(tasks[0]["state"], "outcome_unknown");

    let events = call(
        &observer,
        "session.events",
        json!({"session_id":id,"limit":1000}),
    )
    .await;
    let kinds = host_kinds(&events);
    assert_eq!(count(&kinds, "collaboration_question_asked"), 1);
    assert_eq!(count(&kinds, "collaboration_question_answered"), 0);
    assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Cancelled);

    observer.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
}
