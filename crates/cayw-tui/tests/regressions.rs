//! Reproductions of the six reviewed failures. All IPC is local and offline.
use cayw_agent::AgentEvent;
use cayw_ai::{AssistantEvent, AssistantMessage, Content, Message, Model, StopReason};
use cayw_tui::{
    App, Command,
    driver::Driver,
    transport::{Request, Transport, Update, Watch},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::oneshot,
    time::timeout,
};

fn snapshot(state: &str, cursor: u64) -> Value {
    json!({"id":"s1","label":"session","state":state,"cursor":cursor,"messages":[],"attempt":null,"pending_approvals":[],"unresolved_calls":[],"frozen":false,"binding":null})
}
fn attempt() -> Value {
    json!({"session_id":"s1","attempt_id":"a1","worker_id":"fixture-worker","fence":1})
}
fn envelope(cursor: u64, attempt: Value, event: Value) -> Value {
    json!({"protocol_version":cayw_core::PROTOCOL_VERSION,"session_id":"s1",
        "cursor":cursor,"timestamp":cursor,"attempt":attempt,"event":event})
}
fn started(cursor: u64) -> Value {
    envelope(
        cursor,
        attempt(),
        json!({"type":"attempt_started","lease":attempt()}),
    )
}
fn assistant(timestamp: u64, text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&Model::new("offline", "regression"));
    message.timestamp = timestamp;
    message.content = vec![Content::text(text)];
    message
}
fn update(cursor: u64, timestamp: u64, text: &str) -> Value {
    let message = assistant(timestamp, text);
    envelope(
        cursor,
        attempt(),
        json!({"type":"agent","event":AgentEvent::MessageUpdate {
            message: message.clone(),
            event: Box::new(AssistantEvent::TextDelta { index: 0, delta: text.into(), partial: message }),
        }}),
    )
}
fn end(cursor: u64, timestamp: u64, text: &str) -> Value {
    let mut message = assistant(timestamp, text);
    message.stop_reason = StopReason::Stop;
    envelope(
        cursor,
        attempt(),
        json!({"type":"agent","event":AgentEvent::MessageEnd {
            message: Message::Assistant(message),
        }}),
    )
}
fn app(value: Value) -> App {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(value, true).unwrap();
    app
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}
fn events(after: u64, result: Result<Value, String>) -> Update {
    Update::Events {
        watch: Watch {
            session_id: "s1".into(),
            after,
        },
        result,
    }
}

#[test]
fn terminal_events_refresh_attached_calls_instead_of_leaving_send_blocked_forever() {
    let mut value = snapshot("running", 10);
    value["unresolved_calls"] = json!([{"call":{"id":"c1","name":"file.write","arguments":{}},"attempt_id":"a1","state":"started","execution_started":true,"result":null}]);
    let mut app = app(value);
    let mut driver = Driver::default();
    let commands = driver.update(&mut app, events(10, Ok(json!([
        {"cursor":11,"event":{"type":"agent","event":{"type":"tool_execution_end","call":{"id":"c1","name":"file.write","arguments":{}},"result":{"content":[],"is_error":false,"outcome_unknown":false}}}},
        {"cursor":12,"event":{"type":"agent","event":{"type":"message_end","message":{"role":"toolResult","tool_call_id":"c1","tool_name":"file.write","content":[],"outcome_unknown":false}}}},
        {"cursor":13,"event":{"type":"attempt_finished","lease":{}}}
    ]))));
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: "s1".into()
        }]
    );
    assert!(
        !app.selected().unwrap().can_prompt(),
        "event claims alone cannot settle effects"
    );
    app.upsert_snapshot(snapshot("idle", 13), false).unwrap();
    assert!(app.selected().unwrap().can_prompt());
}

#[test]
fn rejected_prompts_keep_the_original_draft_without_automatic_resend() {
    for error in [
        "Disconnected; request was not sent",
        "IPC request queue unavailable",
        "queue_full: runtime task queue is full (outcome_unknown=false)",
        "busy: another client's run is active (outcome_unknown=false)",
    ] {
        let mut app = app(snapshot("idle", 1));
        app.input.insert("original question 你好");
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
                .as_slice(),
            [Command::Prompt { .. }]
        ));
        app.request_failed("s1", "session.prompt", error);
        assert_eq!(app.input.text(), "original question 你好", "{error}");
        assert!(app.selected().unwrap().messages.is_empty());
        assert!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
                .is_empty()
        );
    }
}

#[test]
fn event_read_failure_invalidates_send_fork_and_approval_until_resync() {
    for result in [
        Err("session.events failed".into()),
        Ok(json!([{"cursor":3,"event":{"type":"agent"}}])),
    ] {
        let mut app = app(snapshot("idle", 2));
        app.apply_events(
            "s1",
            json!([{"cursor":2,"event":{"type":"agent","event":{"type":"turn_start"}}}]),
        )
        .unwrap();
        let mut driver = Driver::default();
        driver.update(&mut app, events(2, result.clone()));
        assert!(!app.selected().unwrap().synced);
        assert_eq!(app.selected().unwrap().cursor, 2);
        app.input.insert("must stay a draft");
        assert!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
                .is_empty()
        );
        assert!(driver.watch(&app).is_none());

        let mut frozen = snapshot("idle", 2);
        frozen["frozen"] = json!(true);
        app.upsert_snapshot(frozen, false).unwrap();
        driver.update(&mut app, events(2, result.clone()));
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(app.dialog.is_none());

        let mut approval = snapshot("awaiting_approval", 2);
        approval["pending_approvals"] = json!([{"id":"a1"}]);
        app.upsert_snapshot(approval, false).unwrap();
        driver.update(&mut app, events(2, result));
        assert!(
            app.handle_key(key(KeyCode::Char('o'), KeyModifiers::ALT))
                .is_empty()
        );
    }
}

#[test]
fn newer_snapshot_hides_stream_until_catchup_and_does_not_skip_unseen_events() {
    let mut initial = snapshot("running", 10);
    initial["attempt"] = attempt();
    let mut app = app(initial);
    let mut driver = Driver::default();
    app.apply_events("s1", json!([started(1), update(11, 100, "partial answer")]))
        .unwrap();
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("partial answer")
    );
    let mut newer = snapshot("awaiting_approval", 14);
    newer["attempt"] = attempt();
    let approval = json!({"id":"a1","session_id":"s1","attempt_id":"a1",
        "input":{"task_id":"fixture-task","target":{"type":"tool_call","call_id":"c1"},
            "capability":"file.write","scope":"fixture.txt","reason":"fixture permission"},
        "requested_at":14,"decision":null});
    newer["pending_approvals"] = json!([approval.clone()]);
    app.upsert_snapshot(newer, false).unwrap();
    assert!(
        app.selected().unwrap().streaming_text.is_none(),
        "unread events may contain the terminal for the consumed partial"
    );
    assert_eq!(driver.watch(&app).unwrap().after, 11);
    driver.update(
        &mut app,
        events(
            11,
            Ok(json!([
                update(12, 100, "partial answer continued"),
                envelope(
                    13,
                    attempt(),
                    json!({"type":"host","kind":"stream_checkpoint","data":{}})
                )
            ])),
        ),
    );
    assert_eq!(driver.watch(&app).unwrap().after, 13);
    assert!(app.selected().unwrap().streaming_text.is_none());
    driver.update(
        &mut app,
        events(
            13,
            Ok(json!([envelope(
                14,
                attempt(),
                json!({"type":"approval_requested","approval":approval})
            )])),
        ),
    );
    assert_eq!(driver.watch(&app).unwrap().after, 14);
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("partial answer continued")
    );
    assert_eq!(app.selected().unwrap().approvals, vec![approval]);
    app.apply_events("s1", json!([end(15, 100, "partial answer continued")]))
        .unwrap();
    assert!(app.selected().unwrap().streaming_text.is_none());
    assert_eq!(app.selected().unwrap().messages.len(), 1);
    assert_eq!(app.selected().unwrap().messages[0]["role"], "assistant");
    assert_eq!(
        app.selected().unwrap().messages[0]["content"][0]["text"],
        "partial answer continued"
    );
}

#[test]
fn session_frozen_rpc_error_blocks_prompt_and_enables_only_confirmed_fork_after_refresh() {
    let mut app = app(snapshot("idle", 2));
    app.input.insert("question");
    app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    app.request_failed(
        "s1",
        "session.prompt",
        "session_frozen: plugin unavailable (outcome_unknown=false)",
    );
    assert!(app.selected().unwrap().frozen);
    assert!(!app.selected().unwrap().can_prompt());
    assert!(!app.selected().unwrap().can_fork(), "resync still required");
}

#[test]
fn rejected_and_successful_prompt_responses_never_overwrite_newer_or_other_session_drafts() {
    for error in [
        "busy: active (outcome_unknown=false)",
        "connection_lost: failed (outcome_unknown=true)",
    ] {
        let mut app = app(snapshot("idle", 1));
        app.input.insert("submitted question");
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.input.clear();
        app.input.insert("newer draft");
        app.request_failed("s1", "session.prompt", error);
        assert_eq!(app.input.text(), "newer draft");
    }
    let mut app = app(snapshot("idle", 1));
    app.input.insert("submitted question");
    app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    let mut second = snapshot("idle", 0);
    second["id"] = json!("s2");
    app.upsert_snapshot(second, false).unwrap();
    app.focus = cayw_tui::Focus::Sessions;
    app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
    app.input.insert("second session draft");
    app.prompt_completed("s1");
    assert_eq!(app.input.text(), "second session draft");
    app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "");
}

#[test]
fn unknown_prompt_outcome_retains_text_and_stays_locked_after_an_idle_snapshot() {
    let mut app = app(snapshot("idle", 1));
    app.input.insert("uncertain question");
    app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    app.request_failed(
        "s1",
        "session.prompt",
        "connection_lost: request admitted (outcome_unknown=true)",
    );
    app.upsert_snapshot(snapshot("idle", 2), false).unwrap();
    assert_eq!(app.input.text(), "uncertain question");
    assert!(!app.selected().unwrap().can_prompt());
    assert!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    let mut confirmed = snapshot("idle", 3);
    confirmed["messages"] = json!([{"role":"user","content":[{"type":"text","text":"uncertain question"}],"timestamp":1}]);
    app.upsert_snapshot(confirmed, false).unwrap();
    assert_eq!(app.input.text(), "uncertain question");
    assert!(
        !app.selected().unwrap().can_prompt(),
        "history text cannot prove ownership of a lost RPC"
    );
}

#[test]
fn unrelated_terminal_event_cannot_erase_calls_or_stream_but_valid_snapshot_can_settle_them() {
    let mut value = snapshot("running", 10);
    value["attempt"] = attempt();
    let call = json!({"call":{"id":"c1","name":"file.write","arguments":{}},
        "attempt_id":"a1","state":"started","execution_started":true,"result":null});
    value["unresolved_calls"] = json!([call.clone()]);
    let mut app = app(value);
    let mut driver = Driver::default();
    app.apply_events("s1", json!([started(1), update(11, 100, "partial")]))
        .unwrap();
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("partial")
    );
    let other = json!({"session_id":"s1","attempt_id":"other-attempt","worker_id":"other-worker","fence":2});
    // An unrelated terminal has a different lease. Observation timestamps do
    // not identify messages, so even the same timestamp must remain harmless.
    let mut unrelated_end = end(13, 100, "other completed message");
    unrelated_end["attempt"] = other.clone();
    let commands = driver.update(
        &mut app,
        events(
            11,
            Ok(json!([
                envelope(
                    12,
                    attempt(),
                    json!({"type":"agent","event":AgentEvent::ToolExecutionEnd {
                        call: cayw_ai::ToolCall::new("different-call", "file.write", json!({})),
                        result: cayw_agent::ToolResult::text("known completion"),
                        duration_ms: None,
                    }})
                ),
                unrelated_end,
                envelope(
                    14,
                    other.clone(),
                    json!({"type":"attempt_finished","lease":other})
                )
            ])),
        ),
    );
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: "s1".into()
        }]
    );
    assert_eq!(app.selected().unwrap().unresolved_calls, vec![call.clone()]);
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("partial")
    );
    assert!(!app.selected().unwrap().can_prompt());
    let mut authoritative = snapshot("running", 14);
    authoritative["attempt"] = attempt();
    authoritative["messages"] = app.selected().unwrap().messages.clone().into();
    authoritative["unresolved_calls"] = json!([call]);
    app.upsert_snapshot(authoritative, false).unwrap();
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("partial")
    );
    assert!(!app.selected().unwrap().can_prompt());
    let mut settled = snapshot("idle", 15);
    settled["messages"] = app.selected().unwrap().messages.clone().into();
    app.upsert_snapshot(settled, false).unwrap();
    assert!(app.selected().unwrap().streaming_text.is_none());
    assert!(app.selected().unwrap().unresolved_calls.is_empty());
    assert!(app.selected().unwrap().can_prompt());
    app.apply_events(
        "s1",
        json!([
            envelope(
                15,
                attempt(),
                json!({"type":"attempt_recovered","lease":attempt(),
            "unknown_call_ids":[],"not_executed_call_ids":[],"executions":[],"capabilities":[]})
            ),
            update(16, 100, "late abandoned partial")
        ]),
    )
    .unwrap();
    assert!(app.selected().unwrap().streaming_text.is_none());
    assert!(app.selected().unwrap().can_prompt());
}

#[test]
fn catching_up_a_snapshot_never_duplicates_messages_or_reopens_settled_approvals() {
    let mut app = app(snapshot("running", 10));
    let mut later = snapshot("idle", 14);
    let done = json!({"role":"assistant","timestamp":1,"content":[{"type":"text","text":"done"}]});
    later["messages"] = json!([done]);
    app.upsert_snapshot(later, false).unwrap();
    app.apply_events("s1", json!([
        {"cursor":11,"event":{"type":"approval_requested","approval":{"id":"already-decided"}}},
        {"cursor":12,"event":{"type":"agent","event":{"type":"message_update","message":{"timestamp":1,"content":[{"type":"text","text":"partial"}]}}}},
        {"cursor":13,"event":{"type":"agent","event":{"type":"message_end","message":done}}},
        {"cursor":14,"event":{"type":"attempt_finished","lease":{}}}
    ])).unwrap();
    assert_eq!(app.selected().unwrap().cursor, 14);
    assert_eq!(app.selected().unwrap().messages, vec![done]);
    assert!(app.selected().unwrap().approvals.is_empty());
    assert!(app.selected().unwrap().streaming_text.is_none());
    assert!(app.selected().unwrap().can_prompt());
}

#[test]
fn missing_runtime_fields_cannot_silently_thaw_a_frozen_session() {
    let mut frozen = snapshot("idle", 2);
    frozen["frozen"] = json!(true);
    let mut app = app(frozen);
    let mut incomplete = snapshot("idle", 3);
    incomplete.as_object_mut().unwrap().remove("frozen");
    incomplete.as_object_mut().unwrap().remove("binding");
    assert!(app.upsert_snapshot(incomplete, false).is_err());
    assert!(app.selected().unwrap().frozen);
    assert!(!app.selected().unwrap().can_prompt());
}

#[test]
fn malformed_success_response_preserves_prompt_text_and_does_not_unlock_send() {
    let mut app = app(snapshot("idle", 1));
    app.input.insert("submitted");
    app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    let mut driver = Driver::default();
    driver.update(
        &mut app,
        Update::Response {
            request: Request {
                tag: 1,
                method: "session.prompt",
                params: json!({"session_id":"s1","text":"submitted"}),
            },
            result: Ok(json!({"id":"s1"})),
        },
    );
    assert_eq!(app.input.text(), "submitted");
    app.upsert_snapshot(snapshot("idle", 2), false).unwrap();
    assert!(!app.selected().unwrap().can_prompt());
}

async fn reply(writer: &mut tokio::net::tcp::OwnedWriteHalf, id: &Value, result: Value) {
    writer
        .write_all((json!({"version":1,"id":id,"result":result}).to_string() + "\n").as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn shutdown_of_prompt_rejected_busy_never_sends_session_cancel() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (rejected, rejection) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = socket.into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut rejected = Some(rejected);
        let mut methods = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let call: Value = serde_json::from_str(&line).unwrap();
            let method = call["method"].as_str().unwrap();
            match method {
                "hello" => reply(&mut writer, &call["id"], json!({"version":1,"max_frame_bytes":1048576,"max_in_flight":64,"max_requests_per_connection":65536,"max_id_bytes":128})).await,
                "session.prompt" => {
                    methods.push("prompt_rejected_busy");
                    // Busy has already been decided, but the response can still be in flight
                    // when the terminal exits. No run ownership has been established.
                    rejected.take().unwrap().send(()).unwrap();
                    tokio::task::yield_now().await;
                    let response = json!({"version":1,"id":call["id"],"error":{"code":"busy","message":"another client's run is active"}}).to_string() + "\n";
                    let _ = writer.write_all(response.as_bytes()).await;
                }
                "session.cancel" => {
                    methods.push("session.cancel");
                    let response = json!({"version":1,"id":call["id"],"result":{"cancelled":true}}).to_string() + "\n";
                    let _ = writer.write_all(response.as_bytes()).await;
                }
                _ => {}
            }
        }
        methods
    });
    let mut transport = Transport::start(address, "fixture-token".into());
    timeout(Duration::from_secs(3), async {
        while !matches!(transport.next().await, Some(Update::Connected)) {}
    })
    .await
    .unwrap();
    transport
        .call(Request {
            tag: 42,
            method: "session.prompt",
            params: json!({"session_id":"s1","text":"question"}),
        })
        .unwrap();
    timeout(Duration::from_secs(3), rejection)
        .await
        .unwrap()
        .unwrap();
    transport.shutdown().await;
    let methods = timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(methods, vec!["prompt_rejected_busy"]);
}
