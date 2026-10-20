//! Independent-review regressions through the actual Runtime and loopback RPC.
//! Only the model input is deterministic; session/task/event state is produced by Runtime.
mod support;

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::SessionState;
use cayw_ipc::{Client, RpcHandler, ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, TaskState};
use cayw_tui::{
    App, Command,
    driver::Driver,
    transport::{Transport, Update},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};

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
    let handler: Arc<dyn RpcHandler> = Arc::new(runtime);
    let task = tokio::spawn(async move {
        serve(
            listener,
            handler,
            ServerConfig::new("review-offline-token"),
            signal,
        )
        .await
    });
    (address, stop, task)
}

async fn next(transport: &mut Transport, stage: &str) -> Update {
    let update = timeout(Duration::from_secs(8), transport.next())
        .await
        .unwrap_or_else(|error| panic!("{stage}: waiting for transport update: {error}"))
        .unwrap_or_else(|| panic!("{stage}: transport updates closed"));
    eprintln!("TRANSPORT_UPDATE stage={stage} update={update:?}");
    update
}

fn attached(value: Value) -> App {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(value, true).unwrap();
    app
}

#[tokio::test]
async fn queued_cancelled_rpc_unblocks_only_after_authoritative_idle_get() {
    let fixture = support::OfflineRuntime::new();
    let runtime = fixture.runtime.clone();
    assert!(runtime.get(&fixture.frozen_id).unwrap().frozen);
    assert!(!fixture.directory.path().join("fixture-output.txt").exists());
    let (address, stop, server_task) = server(runtime.clone()).await;
    let owner = Client::connect(address, "review-offline-token")
        .await
        .unwrap();
    let mut blockers = Vec::new();
    let mut ids = Vec::new();
    for index in 0..4 {
        let id = runtime.create(Some(format!("blocker {index}"))).unwrap().id;
        let client = owner.clone();
        let run_id = id.clone();
        ids.push(id);
        blockers.push(tokio::spawn(async move {
            client
                .request(
                    "session.prompt",
                    json!({"session_id":run_id,"text":"hold concurrency slot"}),
                )
                .await
        }));
    }
    timeout(Duration::from_secs(8), async {
        while !ids
            .iter()
            .all(|id| !runtime.get(id).unwrap().pending_approvals.is_empty())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let session = runtime.create(Some("queued TUI".into())).unwrap();
    let id = session.id.clone();
    let mut app = attached(serde_json::to_value(session).unwrap());
    app.input.insert("queued draft 你好");
    let mut driver = Driver::default();
    let mut transport = Transport::start(address, "review-offline-token".into());
    while !matches!(
        next(&mut transport, "connect queued session").await,
        Update::Connected
    ) {}
    let commands = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    driver.dispatch(&mut app, &transport, commands);
    timeout(Duration::from_secs(8), async {
        while runtime.tasks(&id).unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(runtime.get(&id).unwrap().attempt.is_none());
    let commands = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(
        commands,
        vec![Command::Cancel {
            session_id: id.clone()
        }]
    );
    driver.dispatch(&mut app, &transport, commands);
    let error = loop {
        let update = next(&mut transport, "cancel queued prompt").await;
        let error = match &update {
            Update::Response {
                request,
                result: Err(error),
            } if request.method == "session.prompt" => Some(error.clone()),
            _ => None,
        };
        let commands = driver.update(&mut app, update);
        driver.dispatch(&mut app, &transport, commands);
        if let Some(error) = error {
            break error;
        }
    };
    assert!(error.starts_with("cancelled:"), "{error}");
    assert!(error.ends_with(" (outcome_unknown=false)"), "{error}");
    assert!(!app.selected().unwrap().can_prompt());
    loop {
        let update = next(&mut transport, "refresh cancelled session").await;
        let got = matches!(&update, Update::Response { request, result: Ok(_) } if request.method == "session.get");
        let commands = driver.update(&mut app, update);
        driver.dispatch(&mut app, &transport, commands);
        if got {
            break;
        }
    }
    let can_prompt = app.selected().unwrap().can_prompt();
    let terminal = runtime.get(&id).unwrap();
    assert_eq!(terminal.state, SessionState::Idle);
    assert!(terminal.attempt.is_none());
    assert!(terminal.pending_approvals.is_empty());
    assert!(terminal.unresolved_calls.is_empty());
    let tasks = runtime.tasks(&id).unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "refresh must never resubmit the retained draft"
    );
    assert_eq!(tasks[0].state, TaskState::Cancelled);
    assert_eq!(app.input.text(), "queued draft 你好");
    eprintln!(
        "QUEUED_CANCEL error={error:?} can_prompt={can_prompt} terminal={terminal:?} tasks={tasks:?}"
    );
    transport.shutdown().await;
    for id in ids {
        owner
            .request("session.cancel", json!({"session_id":id}))
            .await
            .unwrap();
    }
    for run in blockers {
        timeout(Duration::from_secs(8), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    owner.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
    assert!(
        can_prompt,
        "settled known cancellation must allow an explicit later prompt"
    );
}

struct PagedStream {
    requests: AtomicUsize,
    more: CancellationToken,
}
impl Provider for PagedStream {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            message.content = vec![Content::text("committed history")];
            message.stop_reason = StopReason::Stop;
            return Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
                message,
            }]));
        }
        let more = self.more.clone();
        Box::pin(async_stream::stream! {
            yield AssistantEvent::Start { message: message.clone() };
            for index in 1..=220 {
                if index == 111 {
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            yield AssistantEvent::Error { message: message.failure(StopReason::Aborted, "fixture cancelled") };
                            return;
                        }
                        _ = more.cancelled() => {}
                    }
                }
                message.content = vec![Content::text(format!("paged fixture {index} 你好"))];
                yield AssistantEvent::TextDelta { index: 0, delta: format!("{index}"), partial: message.clone() };
            }
            cancel.cancelled().await;
            yield AssistantEvent::Error { message: message.failure(StopReason::Aborted, "fixture cancelled") };
        })
    }
}

async fn wait_partial(client: &Client, id: &str, text: &str) -> Value {
    let mut stage = "session.get";
    let mut cursor = 0;
    let mut last_event = None;
    timeout(Duration::from_secs(8), async {
        loop {
            stage = "session.get";
            let snapshot = client
                .request("session.get", json!({"session_id":id}))
                .await
                .unwrap();
            cursor = snapshot["cursor"].as_u64().unwrap();
            let mut after = 0;
            loop {
                stage = "session.events";
                let page = client
                    .request(
                        "session.events",
                        json!({"session_id":id,"after":after,"limit":100}),
                    )
                    .await
                    .unwrap();
                let records = page.as_array().unwrap();
                if let Some(record) = records.last() {
                    last_event = Some(record.clone());
                }
                if records
                    .iter()
                    .any(|record| record["event"]["event"]["message"]["content"][0]["text"] == text)
                {
                    stage = "session.get after matching partial";
                    return client
                        .request("session.get", json!({"session_id":id}))
                        .await
                        .unwrap();
                }
                let Some(last) = records.last() else {
                    break;
                };
                after = last["cursor"].as_u64().unwrap();
                if after >= snapshot["cursor"].as_u64().unwrap() {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "waiting for {text:?} in {id} at {stage}, snapshot cursor={cursor}, last event={last_event:?}: {error}"
        )
    })
}

async fn catch_up(client: &Client, app: &mut App) -> Vec<(u64, usize)> {
    timeout(Duration::from_secs(8), async {
        let mut pages = Vec::new();
        let driver = Driver::default();
        loop {
            let watch = driver.watch(app).unwrap();
            let page = client
                .request(
                    "session.events",
                    json!({"session_id":watch.session_id,"after":watch.after,"limit":100}),
                )
                .await
                .unwrap();
            let count = page.as_array().unwrap().len();
            pages.push((watch.after, count));
            app.apply_events(&watch.session_id, page).unwrap();
            let view = app.selected().unwrap();
            if view.cursor < view.snapshot_cursor {
                assert!(
                    view.streaming_text.is_none(),
                    "unread events may settle the previous partial"
                );
            }
            if count == 0 {
                break;
            }
            assert!(
                app.selected().unwrap().cursor > watch.after,
                "event page made no progress"
            );
        }
        pages
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn first_attach_and_refresh_read_all_real_event_pages_without_replaying_history() {
    let directory = tempfile::tempdir().unwrap();
    let more = CancellationToken::new();
    let runtime = Runtime::open(
        directory.path().join("runtime.db"),
        directory.path(),
        Arc::new(PagedStream {
            requests: AtomicUsize::new(0),
            more: more.clone(),
        }),
        Model::new("offline-tui", "paged-review"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let id = runtime.create(Some("paged attachment".into())).unwrap().id;
    let (address, stop, server_task) = server(runtime.clone()).await;
    let client = Client::connect(address, "review-offline-token")
        .await
        .unwrap();
    client
        .request(
            "session.prompt",
            json!({"session_id":id,"text":"seed history"}),
        )
        .await
        .unwrap();
    let runner = client.clone();
    let run_id = id.clone();
    let run = tokio::spawn(async move {
        runner
            .request(
                "session.prompt",
                json!({"session_id":run_id,"text":"stream pages"}),
            )
            .await
    });
    let snapshot = wait_partial(&client, &id, "paged fixture 110 你好").await;
    let committed = snapshot["messages"].clone();
    let snapshot_cursor = snapshot["cursor"].as_u64().unwrap();
    assert!(snapshot_cursor > 100);
    assert!(snapshot.get("streaming_text").is_none());
    let mut app = attached(snapshot.clone());
    app.input.insert("unsent draft");
    app.upsert_snapshot(snapshot, false).unwrap();
    assert_eq!(Driver::default().watch(&app).unwrap().after, 0);
    let first_pages = catch_up(&client, &mut app).await;
    assert_eq!(first_pages[0], (0, 100));
    assert!(
        first_pages.len() >= 3,
        "must also read the final empty page"
    );
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("paged fixture 110 你好")
    );
    assert_eq!(json!(app.selected().unwrap().messages), committed);
    assert_eq!(app.selected().unwrap().cursor, snapshot_cursor);
    more.cancel();
    let newer = wait_partial(&client, &id, "paged fixture 220 你好").await;
    app.upsert_snapshot(newer.clone(), false).unwrap();
    assert_eq!(app.selected().unwrap().cursor, snapshot_cursor);
    assert!(
        app.selected().unwrap().streaming_text.is_none(),
        "a snapshot ahead of consumed events hides the previous partial until catchup"
    );
    let refresh_pages = catch_up(&client, &mut app).await;
    assert_eq!(refresh_pages[0], (snapshot_cursor, 100));
    assert!(refresh_pages.len() >= 3);
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("paged fixture 220 你好")
    );
    assert_eq!(json!(app.selected().unwrap().messages), newer["messages"]);
    assert_eq!(
        app.selected().unwrap().cursor,
        newer["cursor"].as_u64().unwrap()
    );
    assert_eq!(
        app.selected().unwrap().cursor,
        app.selected().unwrap().snapshot_cursor
    );
    assert_eq!(app.input.text(), "unsent draft");
    assert_eq!(
        runtime.tasks(&id).unwrap().len(),
        2,
        "reading never submits a prompt"
    );
    eprintln!(
        "ATTACH_PAGES first={first_pages:?} refresh={refresh_pages:?} snapshot_cursor={} consumed={} streaming={:?}",
        app.selected().unwrap().snapshot_cursor,
        app.selected().unwrap().cursor,
        app.selected().unwrap().streaming_text
    );
    client
        .request("session.cancel", json!({"session_id":id}))
        .await
        .unwrap();
    let terminal = timeout(Duration::from_secs(8), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    app.upsert_snapshot(terminal.clone(), false).unwrap();
    assert!(
        app.selected().unwrap().streaming_text.is_none(),
        "terminal snapshot immediately hides the ended attempt"
    );
    catch_up(&client, &mut app).await;
    assert!(app.selected().unwrap().streaming_text.is_none());
    assert_eq!(
        json!(app.selected().unwrap().messages),
        terminal["messages"]
    );
    assert!(app.selected().unwrap().can_prompt());
    client.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
}
