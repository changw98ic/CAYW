//! State-only counterexamples. Runtime/IPC evidence lives in the integration tests.
use cayw_agent::AgentEvent;
use cayw_ai::{AssistantEvent, AssistantMessage, Content, Message, Model, StopReason};
use cayw_tui::{
    App, Command,
    driver::Driver,
    transport::{Request, Update},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

fn attempt() -> Value {
    json!({"session_id":"s1","attempt_id":"current","worker_id":"worker","fence":2})
}

fn record(cursor: u64, event: Value) -> Value {
    json!({"protocol_version":cayw_core::PROTOCOL_VERSION,"session_id":"s1",
        "cursor":cursor,"timestamp":cursor,"attempt":attempt(),"event":event})
}

fn started(cursor: u64) -> Value {
    record(cursor, json!({"type":"attempt_started","lease":attempt()}))
}

fn snapshot(state: &str, cursor: u64) -> Value {
    json!({"id":"s1","label":"review regression","state":state,"cursor":cursor,
        "messages":[],"attempt":if state == "idle" { Value::Null } else { attempt() },"pending_approvals":[],"unresolved_calls":[],
        "frozen":false,"binding":null})
}

fn attached(value: Value) -> App {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(value, true).unwrap();
    app
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Command> {
    app.handle_key(KeyEvent::new(code, modifiers))
}

fn assistant(timestamp: u64, text: &str) -> Value {
    let mut message = AssistantMessage::new(&Model::new("offline", "review-regression"));
    message.timestamp = timestamp;
    message.content = vec![Content::text(text)];
    message.stop_reason = StopReason::Stop;
    serde_json::to_value(Message::Assistant(message)).unwrap()
}

fn agent(cursor: u64, kind: &str, message: Value) -> Value {
    let Message::Assistant(mut message) = serde_json::from_value(message).unwrap() else {
        panic!("assistant fixture required");
    };
    let event = match kind {
        "message_update" => {
            message.stop_reason = StopReason::Pending;
            AgentEvent::MessageUpdate {
                event: Box::new(AssistantEvent::TextDelta {
                    index: 0,
                    delta: cayw_ai::content_text(&message.content),
                    partial: message.clone(),
                }),
                message,
            }
        }
        "message_end" => AgentEvent::MessageEnd {
            message: Message::Assistant(message),
        },
        _ => panic!("unsupported fixture agent event"),
    };
    record(cursor, json!({"type":"agent","event":event}))
}

#[test]
fn known_queued_cancellation_releases_prompt_only_after_settled_snapshot() {
    let mut app = attached(snapshot("idle", 3));
    app.input.insert("queued draft");
    assert_eq!(key(&mut app, KeyCode::Enter, KeyModifiers::NONE).len(), 1);
    assert_eq!(
        key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL).len(),
        1
    );
    let error = cayw_ipc::ClientError {
        code: "cancelled".into(),
        message: "task cancelled before execution".into(),
        outcome_unknown: false,
    };
    let commands = Driver::default().update(
        &mut app,
        Update::Response {
            request: Request {
                tag: 1,
                method: "session.prompt",
                params: json!({"session_id":"s1"}),
            },
            result: Err(error.to_string()),
        },
    );
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: "s1".into()
        }]
    );
    assert!(!app.selected().unwrap().can_prompt());
    app.upsert_snapshot(snapshot("idle", 3), false).unwrap();
    assert!(app.selected().unwrap().can_prompt());
    assert_eq!(app.input.text(), "queued draft");
    assert!(app.selected().unwrap().streaming_text.is_none());
}

#[test]
fn cancellation_with_unknown_or_missing_outcome_never_clears_uncertain_prompt() {
    for error in [
        "cancelled: task cancellation uncertain (outcome_unknown=true)",
        "cancelled: missing outcome",
        "runtime_outcome_unknown: unresolved write (outcome_unknown=false)",
        "recovery_required: unfinished bookkeeping (outcome_unknown=false)",
        "runtime_error: unexpected failure (outcome_unknown=false)",
    ] {
        let mut app = attached(snapshot("idle", 3));
        app.input.insert("submitted");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.input.insert(" plus edit");
        app.request_failed("s1", "session.prompt", error);
        app.upsert_snapshot(snapshot("idle", 4), false).unwrap();
        assert!(!app.selected().unwrap().can_prompt(), "{error}");
        assert!(key(&mut app, KeyCode::Enter, KeyModifiers::NONE).is_empty());
        assert_eq!(app.input.text(), "submitted plus edit");
    }
}

#[test]
fn fresh_attach_reads_unfinished_message_before_snapshot_cursor() {
    let mut app = attached(snapshot("running", 13));
    let after = Driver::default().watch(&app).unwrap().after;
    let history = [
        started(1),
        agent(
            13,
            "message_update",
            assistant(100, "streamed fixture 你好"),
        ),
    ];
    let page = history
        .into_iter()
        .filter(|record| record["cursor"].as_u64().unwrap() > after)
        .collect();
    app.apply_events("s1", Value::Array(page)).unwrap();
    let view = app.selected().unwrap();
    assert_eq!(
        view.streaming_text.as_deref(),
        Some("streamed fixture 你好")
    );
    assert_eq!(view.snapshot_cursor, 13);
    assert_eq!(view.cursor, 13);
    assert!(view.messages.is_empty());
}

#[test]
fn snapshot_refresh_keeps_unread_cursor_and_recovered_stream() {
    let mut app = attached(snapshot("running", 13));
    app.upsert_snapshot(snapshot("running", 14), false).unwrap();
    let after = Driver::default().watch(&app).unwrap().after;
    assert!(after < 13, "refresh must retain the unread stream position");
    app.apply_events(
        "s1",
        json!([
            started(1),
            agent(13, "message_update", assistant(100, "first partial"))
        ]),
    )
    .unwrap();
    app.upsert_snapshot(snapshot("running", 15), false).unwrap();
    assert_eq!(Driver::default().watch(&app).unwrap().after, 13);
    assert!(app.selected().unwrap().streaming_text.is_none());
    app.apply_events(
        "s1",
        json!([agent(
            14,
            "message_update",
            assistant(100, "latest partial")
        )]),
    )
    .unwrap();
    assert!(app.selected().unwrap().streaming_text.is_none());
    app.apply_events("s1", json!([{"cursor":15,"event":{"type":"checkpoint"}}]))
        .unwrap();
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("latest partial")
    );
    assert_eq!(app.selected().unwrap().snapshot_cursor, 15);
}

#[test]
fn catchup_pages_preserve_snapshot_history_approvals_frozen_and_unknown() {
    for state in ["running", "needs_reconciliation"] {
        let completed = assistant(10, "already completed");
        let mut value = snapshot(state, 205);
        value["messages"] = json!([completed.clone()]);
        value["frozen"] = json!(true);
        value["binding"] =
            json!({"plugin":"unavailable","frozen":true,"reason":"host unloaded plugin"});
        let approval = json!({"id":"current-approval","session_id":"s1","attempt_id":"current",
            "input":{"task_id":"fixture-task","target":{"type":"tool_call","call_id":"current-call"},
                "capability":"write","scope":"fixture.txt","reason":"fixture permission"},
            "requested_at":203,"decision":null});
        let mut old_approval = approval.clone();
        old_approval["id"] = json!("old-approval");
        old_approval["requested_at"] = json!(99);
        let mut decided = old_approval.clone();
        decided["decision"] = json!({"decision":"approve","actor":"fixture-host","decided_at":101});
        value["pending_approvals"] = json!([approval.clone()]);
        value["unresolved_calls"] = json!([{"call":{"id":"current-call","name":"write","arguments":{}},
            "attempt_id":"current","state":"outcome_unknown","execution_started":true,"result":null}]);
        let expected = value.clone();
        let mut app = attached(value);
        let records: Vec<Value> = (1..=205)
            .map(|cursor| match cursor {
                1 => started(cursor),
                20 => agent(cursor, "message_update", assistant(10, "already")),
                21 => agent(cursor, "message_end", completed.clone()),
                99 => record(
                    cursor,
                    json!({"type":"approval_requested","approval":old_approval}),
                ),
                100 => record(
                    cursor,
                    json!({"type":"host","kind":"plugin_resumed","data":{}}),
                ),
                101 => record(
                    cursor,
                    json!({"type":"approval_decided","approval":decided}),
                ),
                203 => record(
                    cursor,
                    json!({"type":"approval_requested","approval":approval}),
                ),
                204 | 205 => agent(cursor, "message_update", assistant(100, "unfinished")),
                _ => record(
                    cursor,
                    json!({"type":"agent","event":{"type":"turn_start"}}),
                ),
            })
            .collect();
        for (index, page) in records.chunks(100).enumerate() {
            let after = Driver::default().watch(&app).unwrap().after;
            assert_eq!(after, [0, 100, 200][index]);
            let value = Value::Array(page.to_vec());
            app.apply_events("s1", value.clone()).unwrap();
            app.apply_events("s1", value).unwrap();
            let view = app.selected().unwrap();
            assert_eq!(view.snapshot_cursor, 205);
            assert_eq!(
                view.streaming_text.as_deref(),
                if index == 2 { Some("unfinished") } else { None },
                "catchup must hide old text until the authoritative boundary"
            );
            assert_eq!(
                view.messages,
                expected["messages"].as_array().unwrap().clone()
            );
            assert_eq!(
                view.approvals,
                expected["pending_approvals"].as_array().unwrap().clone()
            );
            assert_eq!(
                view.unresolved_calls,
                expected["unresolved_calls"].as_array().unwrap().clone()
            );
            assert_eq!(view.binding.as_ref(), Some(&expected["binding"]));
            assert!(view.frozen);
            assert!(view.has_unknown());
            assert!(view.synced);
            assert!(!view.can_prompt());
            assert!(key(&mut app, KeyCode::Char('o'), KeyModifiers::ALT).is_empty());
        }
        assert_eq!(app.selected().unwrap().cursor, 205);
        assert_eq!(
            app.selected().unwrap().streaming_text.as_deref(),
            Some("unfinished")
        );
        // A terminal snapshot must suppress the same text even before its event
        // arrives, and a late update cannot reopen the recovered lease.
        let mut terminal = expected;
        terminal["cursor"] = json!(206);
        terminal["state"] = json!("needs_reconciliation");
        terminal["attempt"] = Value::Null;
        app.upsert_snapshot(terminal, false).unwrap();
        assert!(app.selected().unwrap().streaming_text.is_none());
        app.apply_events("s1", json!([
            record(206, json!({"type":"attempt_recovered","lease":attempt(),
                "unknown_call_ids":["current-call"],"not_executed_call_ids":[],"executions":[],"capabilities":[]})),
            agent(207, "message_update", assistant(100, "late abandoned output"))
        ])).unwrap();
        assert!(app.selected().unwrap().streaming_text.is_none());
        assert!(app.selected().unwrap().has_unknown());
        assert!(!app.selected().unwrap().can_prompt());
        assert!(!app.selected().unwrap().can_fork());
    }
}
