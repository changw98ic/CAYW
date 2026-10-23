//! State-only event replay tests. Real Runtime crash/pagination probes are run separately.
use cayw_tui::{App, Command, driver::Driver};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

fn lease(name: &str, fence: u64) -> Value {
    json!({"session_id":"s1","attempt_id":name,"worker_id":"worker","fence":fence})
}

fn snapshot(state: &str, cursor: u64, attempt: Value) -> Value {
    json!({"id":"s1","label":"stream replay","state":state,"cursor":cursor,
        "attempt":attempt,"messages":[],"pending_approvals":[],"unresolved_calls":[],
        "frozen":false,"binding":null})
}

fn attach(value: Value) -> App {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(value, true).unwrap();
    app
}

fn message(text: &str) -> Value {
    // Real MessageUpdate carries AssistantMessage without a role tag.
    json!({"timestamp":10,"content":[{"type":"text","text":text}],"stop_reason":"pending"})
}

fn record(cursor: u64, attempt: &Value, event: Value) -> Value {
    json!({"session_id":"s1","cursor":cursor,"attempt":attempt,"event":event})
}

fn start(cursor: u64, attempt: &Value) -> Value {
    record(
        cursor,
        attempt,
        json!({"type":"attempt_started","lease":attempt}),
    )
}

fn agent(cursor: u64, attempt: &Value, event: Value) -> Value {
    record(cursor, attempt, json!({"type":"agent","event":event}))
}

fn update(cursor: u64, attempt: &Value, text: &str) -> Value {
    agent(
        cursor,
        attempt,
        json!({"type":"message_update","message":message(text)}),
    )
}

fn end(cursor: u64, attempt: &Value, text: &str) -> Value {
    let mut completed = message(text);
    completed["role"] = json!("assistant");
    completed["stop_reason"] = json!("stop");
    agent(
        cursor,
        attempt,
        json!({"type":"message_end","message":completed}),
    )
}

fn finish(cursor: u64, attempt: &Value, kind: &str) -> Value {
    record(
        cursor,
        attempt,
        json!({"type":kind,"lease":attempt,"unknown_call_ids":[],"not_executed_call_ids":[]}),
    )
}

fn assert_stream(app: &App, want: Option<&str>, context: &str) {
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        want,
        "{context}"
    );
}

#[test]
fn every_catchup_page_boundary_hides_committed_and_abandoned_messages() {
    let old = lease("old", 1);
    let current = lease("current", 2);
    let records = [
        start(1, &old),
        update(2, &old, "OLD partial"),
        update(3, &old, "OLD final"),
        end(4, &old, "OLD final"),
        finish(5, &old, "attempt_finished"),
        start(6, &current),
        update(7, &current, "CURRENT first"),
        update(8, &current, "CURRENT latest"),
    ];
    let mut value = snapshot("running", 8, current);
    value["messages"] =
        json!([{"role":"assistant","timestamp":10,"content":[{"type":"text","text":"OLD final"}]}]);
    // Every possible partition of this log, including a page after each record.
    for cuts in 0u16..128 {
        let mut app = attach(value.clone());
        app.input.insert("retained draft");
        let mut from = 0;
        for to in 1..=records.len() {
            if to < records.len() && cuts & (1 << (to - 1)) == 0 {
                continue;
            }
            app.apply_events("s1", json!(records[from..to])).unwrap();
            let expected = if to == 8 {
                Some("CURRENT latest")
            } else {
                None
            };
            assert_stream(&app, expected, &format!("partition {cuts}, consumed {to}"));
            assert_eq!(app.selected().unwrap().cursor, to as u64);
            assert_eq!(app.selected().unwrap().snapshot_cursor, 8);
            assert_eq!(
                app.selected().unwrap().messages,
                value["messages"].as_array().unwrap().clone()
            );
            for _ in 0..2 {
                app.set_disconnected("local test reconnect");
                app.set_connected();
                app.upsert_snapshot(value.clone(), false).unwrap();
                assert_stream(&app, expected, "same-position reconnect");
                assert_eq!(Driver::default().watch(&app).unwrap().after, to as u64);
            }
            app.apply_events("s1", json!(records[from..to])).unwrap();
            assert_stream(&app, expected, "duplicate page");
            from = to;
        }
        app.apply_events("s1", json!([])).unwrap();
        assert_stream(&app, Some("CURRENT latest"), "empty page");
        assert_eq!(app.input.text(), "retained draft");
    }
}

#[test]
fn terminal_snapshots_never_publish_the_old_attempt_at_any_page_boundary() {
    let attempt = lease("ended", 1);
    for kind in ["attempt_recovered", "attempt_finished"] {
        for state in ["idle", "needs_reconciliation"] {
            let records = [
                start(1, &attempt),
                update(2, &attempt, "ABANDONED"),
                finish(3, &attempt, kind),
            ];
            let mut value = snapshot(state, 3, Value::Null);
            if state == "needs_reconciliation" {
                value["unresolved_calls"] =
                    json!([{"call":{"id":"unknown"},"state":"outcome_unknown"}]);
            }
            let mut app = attach(value.clone());
            for row in records {
                app.apply_events("s1", json!([row])).unwrap();
                assert_stream(&app, None, kind);
                app.upsert_snapshot(value.clone(), false).unwrap();
                assert_stream(&app, None, "terminal refresh");
                assert_eq!(app.selected().unwrap().can_prompt(), state == "idle");
            }
        }
    }
}

#[test]
fn same_attempt_completed_message_waits_for_its_terminal_before_projection() {
    let attempt = lease("active", 1);
    let records = [
        start(1, &attempt),
        update(2, &attempt, "completed within current attempt"),
        end(3, &attempt, "completed within current attempt"),
        update(4, &attempt, "CURRENT second message"),
    ];
    let mut value = snapshot("running", 4, attempt);
    value["messages"] = json!([{"role":"assistant","timestamp":10,"content":[{"type":"text","text":"completed within current attempt"}]}]);
    let mut app = attach(value);
    for (index, row) in records.into_iter().enumerate() {
        app.apply_events("s1", json!([row])).unwrap();
        assert_stream(
            &app,
            if index == 3 {
                Some("CURRENT second message")
            } else {
                None
            },
            "same-attempt boundary",
        );
    }
}

#[test]
fn refreshed_snapshot_hides_previous_stream_until_unread_terminals_are_consumed() {
    let attempt = lease("active", 1);
    let mut app = attach(snapshot("running", 2, attempt.clone()));
    app.apply_events(
        "s1",
        json!([start(1, &attempt), update(2, &attempt, "first answer")]),
    )
    .unwrap();
    assert_stream(&app, Some("first answer"), "initial attachment");
    app.upsert_snapshot(snapshot("running", 4, attempt.clone()), false)
        .unwrap();
    assert_stream(&app, None, "new snapshot ahead of unread terminal");
    assert_eq!(Driver::default().watch(&app).unwrap().after, 2);
    app.apply_events("s1", json!([end(3, &attempt, "first answer")]))
        .unwrap();
    assert_stream(&app, None, "terminal before boundary");
    app.upsert_snapshot(snapshot("running", 4, attempt.clone()), false)
        .unwrap();
    app.apply_events("s1", json!([update(4, &attempt, "second answer")]))
        .unwrap();
    assert_stream(
        &app,
        Some("second answer"),
        "new unfinished message before snapshot",
    );
    app.upsert_snapshot(snapshot("idle", 5, Value::Null), false)
        .unwrap();
    assert_stream(&app, None, "idle snapshot ahead of end event");
    assert_eq!(app.selected().unwrap().cursor, 4);
    app.apply_events("s1", json!([finish(5, &attempt, "attempt_recovered")]))
        .unwrap();
    assert_stream(&app, None, "recovery consumed");
}

#[test]
fn updates_and_terminals_require_the_entire_current_attempt_identity() {
    let current = lease("active", 2);
    let mut app = attach(snapshot("running", 0, Value::Null));
    app.apply_events(
        "s1",
        json!([start(1, &current), update(2, &current, "CURRENT")]),
    )
    .unwrap();
    let mut cursor = 2;
    for field in ["attempt_id", "session_id", "worker_id", "fence", "missing"] {
        let mut other = current.clone();
        match field {
            "fence" => other[field] = json!(1),
            "missing" => other = Value::Null,
            key => other[key] = json!("different"),
        }
        for event in [
            json!({"type":"message_update","message":message("WRONG attempt")}),
            json!({"type":"message_end","message":{"role":"assistant","timestamp":10,"content":[]}}),
            json!({"type":"agent_end","messages":[]}),
        ] {
            cursor += 1;
            app.apply_events("s1", json!([agent(cursor, &other, event)]))
                .unwrap();
            assert_stream(&app, Some("CURRENT"), field);
        }
        for kind in ["attempt_finished", "attempt_recovered"] {
            cursor += 1;
            app.apply_events("s1", json!([finish(cursor, &other, kind)]))
                .unwrap();
            assert_stream(&app, Some("CURRENT"), field);
        }
    }
    cursor += 1;
    app.apply_events("s1", json!([finish(cursor, &current, "attempt_finished")]))
        .unwrap();
    assert_stream(&app, None, "matching terminal");
    cursor += 1;
    app.apply_events(
        "s1",
        json!([update(cursor, &current, "LATE after terminal")]),
    )
    .unwrap();
    assert_stream(&app, None, "late update cannot reopen ended attempt");
}

#[test]
fn matching_message_agent_and_attempt_terminals_clear_live_stream() {
    let attempt = lease("active", 1);
    for kind in [
        "message_end",
        "agent_end",
        "attempt_finished",
        "attempt_recovered",
    ] {
        let mut app = attach(snapshot("running", 0, Value::Null));
        app.apply_events(
            "s1",
            json!([start(1, &attempt), update(2, &attempt, "pending")]),
        )
        .unwrap();
        assert_stream(&app, Some("pending"), kind);
        let terminal = match kind {
            "message_end" => end(3, &attempt, "pending"),
            "agent_end" => agent(3, &attempt, json!({"type":"agent_end","messages":[]})),
            _ => finish(3, &attempt, kind),
        };
        app.apply_events("s1", json!([terminal])).unwrap();
        assert_stream(&app, None, kind);
    }
}

#[test]
fn message_and_turn_end_clear_current_stream_when_observation_timestamp_changes() {
    let attempt = lease("active", 1);
    for kind in ["message_end", "turn_end"] {
        let mut app = attach(snapshot("running", 0, Value::Null));
        app.apply_events(
            "s1",
            json!([start(1, &attempt), update(2, &attempt, "pending")]),
        )
        .unwrap();
        assert_stream(&app, Some("pending"), kind);
        let mut completed = message("completed");
        completed["timestamp"] = json!(11);
        completed["stop_reason"] = json!("stop");
        if kind == "message_end" {
            completed["role"] = json!("assistant");
        }
        app.apply_events(
            "s1",
            json!([agent(
                3,
                &attempt,
                json!({"type":kind,"message":completed,"tool_results":[]})
            )]),
        )
        .unwrap();
        assert_stream(&app, None, kind);
    }
}

#[test]
fn unknown_frozen_cancel_and_prompt_ownership_survive_catchup_and_reconnect() {
    let attempt = lease("active", 1);
    let mut app = attach(snapshot("idle", 0, Value::Null));
    app.input.insert("owned prompt");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .len(),
        1
    );
    app.task_admitted("s1", "original-task");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        vec![Command::Cancel {
            session_id: "s1".into(),
            task_id: "original-task".into(),
        }]
    );
    let mut value = snapshot("needs_reconciliation", 2, attempt.clone());
    value["frozen"] = json!(true);
    value["binding"] = json!({"plugin":"unavailable","frozen":true});
    value["pending_approvals"] = json!([{"id":"current-approval"}]);
    value["unresolved_calls"] = json!([{"call":{"id":"unknown"},"state":"outcome_unknown"}]);
    app.upsert_snapshot(value.clone(), false).unwrap();
    for row in [start(1, &attempt), update(2, &attempt, "held output")] {
        app.apply_events("s1", json!([row])).unwrap();
        assert!(app.selected().unwrap().request_pending);
        assert!(app.selected().unwrap().cancel_pending);
        assert!(!app.selected().unwrap().can_prompt());
        assert!(!app.selected().unwrap().can_fork());
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT))
                .is_empty()
        );
        assert_eq!(
            app.selected().unwrap().approvals,
            value["pending_approvals"].as_array().unwrap().clone()
        );
        assert_eq!(
            app.selected().unwrap().unresolved_calls,
            value["unresolved_calls"].as_array().unwrap().clone()
        );
        assert_eq!(
            app.selected().unwrap().binding.as_ref(),
            Some(&value["binding"])
        );
    }
    assert_stream(
        &app,
        Some("held output"),
        "unknown does not erase current output",
    );
    for _ in 0..3 {
        app.set_disconnected("local reconnect");
        app.set_connected();
        app.upsert_snapshot(snapshot("idle", 3, Value::Null), false)
            .unwrap();
        assert!(
            !app.selected().unwrap().can_prompt(),
            "unknown owned RPC is unresolved"
        );
        assert_stream(&app, None, "reconnect terminal snapshot");
        assert_eq!(app.selected().unwrap().cursor, 2);
        assert_eq!(app.input.text(), "owned prompt");
    }
}
