//! End-to-end checks use the actual Runtime RPC adapter and durable Core events.
mod support;

use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::SessionState;
use cayw_ipc::{Client, RpcHandler, ServerConfig, serve};
use cayw_tui::{
    App, Command,
    driver::Driver,
    transport::{Transport, Update},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

async fn server(
    handler: Arc<dyn RpcHandler>,
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
            handler,
            ServerConfig::new("runtime-fixture-token"),
            signal,
        )
        .await
    });
    (address, stop, task)
}
async fn next(transport: &mut Transport) -> Update {
    timeout(Duration::from_secs(8), transport.next())
        .await
        .unwrap()
        .unwrap()
}
async fn connected(transport: &mut Transport) {
    while !matches!(next(transport).await, Update::Connected) {}
}
async fn execute(client: &Client, command: Command) -> Value {
    let (method, params) = command.rpc().unwrap();
    client.request(method, params).await.unwrap()
}
fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Command> {
    app.handle_key(KeyEvent::new(code, modifiers))
}

#[tokio::test]
async fn canonical_frozen_view_and_confirmed_fork_use_runtime_not_invented_fixture_fields() {
    let fixture = support::OfflineRuntime::new();
    let source = fixture.frozen_id.clone();
    let runtime = fixture.runtime.clone();
    let (address, stop, task) = server(Arc::new(runtime.clone())).await;
    let client = Client::connect(address, "runtime-fixture-token")
        .await
        .unwrap();
    let value = client
        .request("session.get", json!({"session_id":source}))
        .await
        .unwrap();
    assert_eq!(value["frozen"], true);
    assert_eq!(value["binding"]["module"], "fixture-binding");
    assert_eq!(value["binding"]["frozen"], true);
    assert!(value["binding"]["registry_id"].is_string());
    assert!(
        value["binding"]["reason"]
            .as_str()
            .unwrap()
            .contains("unloaded")
    );
    assert!(value["binding"].get("module_id").is_none());
    assert!(value["binding"].get("available").is_none());
    let baseline = runtime.get(&source).unwrap();
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(value, true).unwrap();
    app.input.insert("cannot run frozen session");
    assert!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE).is_empty());
    assert!(press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL).is_empty());
    assert!(app.dialog.is_some());
    assert!(press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).is_empty());
    assert_eq!(runtime.list().unwrap().len(), 1);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE).is_empty());
    let commands = press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(commands.len(), 1);
    let fork = execute(&client, commands.into_iter().next().unwrap()).await;
    assert_ne!(fork["id"], source);
    assert_eq!(fork["frozen"], false);
    assert_eq!(fork["binding"], Value::Null);
    assert_eq!(
        runtime.get(&source).unwrap(),
        baseline,
        "source and its frozen binding stay intact"
    );

    // A real session_frozen RPC error also protects an older local projection.
    let rejected = client
        .request(
            "session.prompt",
            json!({"session_id":source,"text":"blocked"}),
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.code, "session_frozen");
    app.request_failed(&source, "session.prompt", &rejected.to_string());
    assert!(app.selected().unwrap().frozen);
    assert!(!app.selected().unwrap().synced);
    assert!(!app.selected().unwrap().can_prompt());
    assert!(!app.selected().unwrap().can_fork());
    assert!(!fixture.directory.path().join("fixture-output.txt").exists());
    client.close().await;
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn unknown_core_effects_prevent_fork_in_both_tui_and_actual_runtime() {
    let fixture = support::OfflineRuntime::new();
    let runtime = fixture.runtime.clone();
    let id = fixture.frozen_id.clone();
    let store = runtime.store();
    let lease = store.begin_attempt(&id, "offline-review-worker").unwrap();
    let call = ToolCall::new("unsettled-call", "fixture.write", json!({}));
    let mut assistant = AssistantMessage::new(&Model::new("offline", "fixture"));
    assistant.content = vec![Content::ToolCall(call.clone())];
    assistant.stop_reason = StopReason::ToolUse;
    for event in [
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: Message::Assistant(AssistantMessage::new(&Model::new("offline", "fixture"))),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(assistant),
        },
        AgentEvent::ToolExecutionStart { call: call.clone() },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    store.mark_tool_started(&lease, &call.id).unwrap();
    runtime.recover(&id).unwrap();
    let (address, stop, task) = server(Arc::new(runtime.clone())).await;
    let client = Client::connect(address, "runtime-fixture-token")
        .await
        .unwrap();
    let value = client
        .request("session.get", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(value["state"], "needs_reconciliation");
    assert_eq!(value["unresolved_calls"][0]["state"], "outcome_unknown");
    assert_eq!(value["frozen"], true);
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(value, true).unwrap();
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(app.dialog.is_none());
    assert!(press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).is_empty());
    let result = client
        .request(
            "session.fork",
            json!({"session_id":id,"label":"must not fork","confirm":true,"mode":"chat"}),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(runtime.list().unwrap().len(), 1);
    client.close().await;
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn attached_started_call_unblocks_only_after_authoritative_terminal_snapshot() {
    let fixture = support::OfflineRuntime::new();
    let runtime = fixture.runtime.clone();
    let id = runtime.create(Some("attached".into())).unwrap().id;
    let (address, stop, task) = server(Arc::new(runtime.clone())).await;
    let owner = Client::connect(address, "runtime-fixture-token")
        .await
        .unwrap();
    let owner_call = owner.clone();
    let owner_id = id.clone();
    let running = tokio::spawn(async move {
        owner_call
            .request(
                "session.prompt",
                json!({"session_id":owner_id,"text":"offline tool"}),
            )
            .await
    });
    let attached = timeout(Duration::from_secs(8), async {
        loop {
            let view = runtime.get(&id).unwrap();
            if !view.pending_approvals.is_empty()
                && view
                    .unresolved_calls
                    .iter()
                    .any(|call| call.execution_started)
            {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(serde_json::to_value(&attached).unwrap(), true)
        .unwrap();
    assert!(!app.selected().unwrap().can_prompt());
    let mut driver = Driver::default();
    let mut transport = Transport::start(address, "runtime-fixture-token".into());
    connected(&mut transport).await;
    let approval = &attached.pending_approvals[0];
    owner
        .request(
            "approval.decide",
            json!({"session_id":id,"approval_id":approval.id,"decision":"allow_once"}),
        )
        .await
        .unwrap();
    let terminal = timeout(Duration::from_secs(8), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(terminal["state"], "idle");
    assert_eq!(terminal["unresolved_calls"], json!([]));
    transport.watch(driver.watch(&app)).unwrap();
    let commands = loop {
        let update = next(&mut transport).await;
        if matches!(update, Update::Events { .. }) {
            break driver.update(&mut app, update);
        }
    };
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: id.clone()
        }]
    );
    assert!(!app.selected().unwrap().synced);
    assert!(!app.selected().unwrap().can_prompt());
    assert!(
        !app.selected().unwrap().unresolved_calls.is_empty(),
        "event claims must not erase attached effects"
    );
    driver.dispatch(&mut app, &transport, commands);
    loop {
        let update = next(&mut transport).await;
        let done = matches!(&update, Update::Response { request, result: Ok(_), .. } if request.method == "session.get");
        driver.update(&mut app, update);
        if done {
            break;
        }
    }
    assert!(app.selected().unwrap().synced);
    assert!(app.selected().unwrap().can_prompt());
    assert!(app.selected().unwrap().unresolved_calls.is_empty());
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("fixture-output.txt")).unwrap(),
        "offline terminal verification"
    );
    assert_eq!(runtime.get(&id).unwrap().state, SessionState::Idle);
    transport.shutdown().await;
    owner.close().await;
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn actual_running_snapshot_hides_stream_until_catchup_consumes_every_unseen_envelope() {
    let gate = CancellationToken::new();
    let fixture = support::OfflineRuntime::with_stream_gate(Some(gate.clone()));
    let runtime = fixture.runtime.clone();
    let initial = runtime.create(Some("stream snapshot race".into())).unwrap();
    let id = initial.id.clone();
    let (address, stop, server_task) = server(Arc::new(runtime.clone())).await;
    let client = Client::connect(address, "runtime-fixture-token")
        .await
        .unwrap();
    let runner = client.clone();
    let run_id = id.clone();
    let run = tokio::spawn(async move {
        runner
            .request(
                "session.prompt",
                json!({"session_id":run_id,"text":"stream race"}),
            )
            .await
    });
    let page = timeout(Duration::from_secs(8), async {
        loop {
            let page = client
                .request(
                    "session.events",
                    json!({"session_id":id,"after":initial.cursor,"limit":100}),
                )
                .await
                .unwrap();
            if page
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["event"]["event"]["type"] == "message_update")
            {
                break page;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(serde_json::to_value(initial).unwrap(), true)
        .unwrap();
    app.apply_events(&id, page).unwrap();
    let consumed = app.selected().unwrap().cursor;
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("streamed fixture 你好")
    );
    let lease = runtime.get(&id).unwrap().attempt.unwrap();
    for index in 0..3 {
        runtime
            .store()
            .append_host_event(&lease, "stream_checkpoint", json!({"index":index}))
            .unwrap();
    }
    let newer = client
        .request("session.get", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(newer["cursor"].as_u64().unwrap(), consumed + 3);
    assert!(newer.get("streaming_text").is_none());
    app.upsert_snapshot(newer, false).unwrap();
    let driver = Driver::default();
    assert_eq!(driver.watch(&app).unwrap().after, consumed);
    assert!(
        app.selected().unwrap().streaming_text.is_none(),
        "unread events may contain the terminal for this partial"
    );
    let catchup = client
        .request(
            "session.events",
            json!({"session_id":id,"after":consumed,"limit":100}),
        )
        .await
        .unwrap();
    assert_eq!(catchup.as_array().unwrap().len(), 3);
    for (index, record) in catchup.as_array().unwrap().iter().enumerate() {
        app.apply_events(&id, json!([record])).unwrap();
        assert_eq!(
            driver.watch(&app).unwrap().after,
            consumed + index as u64 + 1
        );
        assert_eq!(
            app.selected().unwrap().streaming_text.as_deref(),
            if index == 2 {
                Some("streamed fixture 你好")
            } else {
                None
            },
            "only the final unseen envelope reaches the snapshot boundary"
        );
    }
    assert_eq!(
        app.selected().unwrap().cursor,
        app.selected().unwrap().snapshot_cursor
    );
    gate.cancel();
    let approval = timeout(Duration::from_secs(8), async {
        loop {
            let view = runtime.get(&id).unwrap();
            if let Some(approval) = view.pending_approvals.first() {
                break approval.id.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    client
        .request(
            "approval.decide",
            json!({"session_id":id,"approval_id":approval,"decision":"allow_once"}),
        )
        .await
        .unwrap();
    let terminal = timeout(Duration::from_secs(8), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(terminal["state"], "idle");
    assert!(terminal["attempt"].is_null());
    app.upsert_snapshot(terminal.clone(), false).unwrap();
    assert!(
        app.selected().unwrap().streaming_text.is_none(),
        "terminal snapshot must hide the ended attempt before its unread events arrive"
    );
    let rest = client
        .request(
            "session.events",
            json!({"session_id":id,"after":app.selected().unwrap().cursor,"limit":100}),
        )
        .await
        .unwrap();
    assert!(!rest.as_array().unwrap().is_empty());
    for record in rest.as_array().unwrap() {
        app.apply_events(&id, json!([record])).unwrap();
        assert!(
            app.selected().unwrap().streaming_text.is_none(),
            "terminal catchup must never resurrect ended text"
        );
        assert_eq!(
            json!(app.selected().unwrap().messages),
            terminal["messages"]
        );
    }
    assert_eq!(
        app.selected().unwrap().cursor,
        app.selected().unwrap().snapshot_cursor
    );
    assert!(app.selected().unwrap().can_prompt());
    client.close().await;
    stop.cancel();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn busy_other_client_run_survives_rejected_tui_shutdown() {
    let fixture = support::OfflineRuntime::new();
    let runtime = fixture.runtime.clone();
    let id = runtime
        .create(Some("owned by another client".into()))
        .unwrap()
        .id;
    let (address, stop, task) = server(Arc::new(runtime.clone())).await;
    let owner = Client::connect(address, "runtime-fixture-token")
        .await
        .unwrap();
    let owner_call = owner.clone();
    let owner_id = id.clone();
    let run = tokio::spawn(async move {
        owner_call
            .request(
                "session.prompt",
                json!({"session_id":owner_id,"text":"other client's run"}),
            )
            .await
    });
    timeout(Duration::from_secs(8), async {
        while runtime.get(&id).unwrap().pending_approvals.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let baseline = runtime.get(&id).unwrap();
    let mut transport = Transport::start(address, "runtime-fixture-token".into());
    connected(&mut transport).await;
    // The TUI attached before the owner's run started, so its local idle view is stale.
    let mut stale = serde_json::to_value(&baseline).unwrap();
    stale["state"] = json!("idle");
    stale["pending_approvals"] = json!([]);
    stale["unresolved_calls"] = json!([]);
    let original_messages = stale["messages"].clone();
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(stale, true).unwrap();
    app.input.insert("rejected draft");
    let mut driver = Driver::default();
    let commands = press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    driver.dispatch(&mut app, &transport, commands);
    loop {
        let update = next(&mut transport).await;
        if let Update::Response { request, result } = &update
            && request.method == "session.prompt"
        {
            assert!(result.as_ref().unwrap_err().starts_with("busy:"));
            let commands = driver.update(&mut app, update);
            assert_eq!(
                commands,
                vec![Command::Get {
                    session_id: id.clone()
                }]
            );
            break;
        }
    }
    assert_eq!(app.input.text(), "rejected draft");
    assert_eq!(json!(app.selected().unwrap().messages), original_messages);
    assert!(!app.selected().unwrap().can_prompt());
    transport.shutdown().await;
    assert_eq!(runtime.get(&id).unwrap(), baseline);
    assert!(!run.is_finished());
    assert!(
        runtime
            .tasks(&id)
            .unwrap()
            .iter()
            .all(|task| !task.cancel_requested)
    );
    owner
        .request("session.cancel", json!({"session_id":id}))
        .await
        .unwrap();
    let _ = timeout(Duration::from_secs(8), run).await.unwrap().unwrap();
    owner.close().await;
    stop.cancel();
    task.await.unwrap().unwrap();
}
