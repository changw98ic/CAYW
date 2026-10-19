use cayw_tui::{App, Command, Decision, Editor, Focus, SessionView, sanitize};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

fn snapshot(state: &str, cursor: u64) -> Value {
    json!({
        "id": "s1", "label": "fixture session", "cursor": cursor,
        "state": state, "messages": [], "attempt": null,
        "pending_approvals": [], "unresolved_calls": [], "frozen":false, "binding":null
    })
}

fn app(state: &str) -> App {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(snapshot(state, 0), true).unwrap();
    app
}

#[test]
fn unicode_editing_never_splits_a_character_and_preserves_multiline_paste() {
    let mut editor = Editor::default();
    editor.insert("你🙂e\u{301}\nnext");
    editor.home();
    editor.left();
    editor.backspace();
    assert_eq!(editor.text(), "你🙂\nnext");
    editor.right();
    editor.end();
    editor.left();
    editor.delete();
    assert_eq!(editor.text(), "你🙂\nnex");
    editor.insert("好");
    assert_eq!(editor.text(), "你🙂\nnex好");
}

#[test]
fn terminal_controls_are_removed_but_unicode_and_newlines_survive() {
    assert_eq!(
        sanitize("你好\x1b[31mRED\x1b[0m\x1b]52;c;secret\x07\r\n\t🙂\x08\u{9b}2J"),
        "你好RED\n    🙂"
    );
    assert_eq!(sanitize("a\x1bPpayload\x1b\\b\u{202e}c"), "abc");
}

#[test]
fn stream_cursor_deduplicates_and_invalid_records_do_not_consume_cursor() {
    let mut app = app("running");
    let attempt = json!({"session_id":"s1","attempt_id":"a1","worker_id":"worker","fence":1});
    let records = json!([
        {"cursor":1,"attempt":attempt,"event":{"type":"attempt_started","lease":attempt}},
        {"cursor": 2, "attempt":attempt, "event": {"type":"agent", "event":{"type": "message_update", "message": {"content": [{"type":"text", "text":"你"} ]}}}},
        {"cursor": 3, "attempt":attempt, "event": {"type":"agent", "event":{"type": "message_update", "message": {"content": [{"type":"text", "text":"你好"} ]}}}}
    ]);
    app.apply_events("s1", records.clone()).unwrap();
    app.apply_events("s1", records).unwrap();
    let session = app.selected().unwrap();
    assert_eq!(session.cursor, 3);
    assert_eq!(session.streaming_text.as_deref(), Some("你好"));
    let bad = json!([{"cursor":4}]);
    assert!(app.apply_events("s1", bad).is_err());
    assert_eq!(app.selected().unwrap().cursor, 3);
}

#[test]
fn new_prompt_is_dispatched_once_and_cancel_does_not_claim_a_terminal_state() {
    let mut app = app("idle");
    app.focus = Focus::Composer;
    app.input.insert("你好");
    let send = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(&send[..], [Command::Prompt { session_id, text }] if session_id == "s1" && text == "你好")
    );
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    let cancel = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(
        cancel,
        vec![Command::Cancel {
            session_id: "s1".into()
        }]
    );
    assert!(!app.selected().unwrap().can_prompt());
    app.cancel_acknowledged("s1", true);
    assert!(!app.selected().unwrap().can_prompt());
}

#[test]
fn unknown_effects_block_both_send_and_fork_without_clearance_button() {
    let mut value = snapshot("needs_reconciliation", 4);
    value["unresolved_calls"] = json!([{"id": "call1"}]);
    let view = SessionView::from_snapshot(value).unwrap();
    assert!(!view.can_prompt());
    assert!(!view.can_fork());
    let mut app = app("idle");
    app.upsert_snapshot(snapshot("needs_reconciliation", 4), true)
        .unwrap();
    app.input.insert("do it again");
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .is_empty()
    );
}

#[test]
fn frozen_session_requires_explicit_confirmation_before_fork() {
    let mut app = app("idle");
    let mut value = snapshot("idle", 0);
    value["frozen"] = json!(true);
    value["binding"] = json!({"module":"fixture-binding", "version":"1.0.0", "registry_id":"f4d8cc10-9530-4536-ac3c-1cf08e8d980d", "generation":1, "manifest":{}, "frozen":true, "reason":"host unloaded module", "workflow_state":null});
    app.upsert_snapshot(value, true).unwrap();
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .is_empty()
    );
    assert!(app.dialog.is_some());
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(app.dialog.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        vec![Command::Fork {
            session_id: "s1".into(),
            label: "fixture session (chat fork)".into()
        }]
    );
}

#[test]
fn approval_choices_use_pending_id_and_remain_pending_until_server_acknowledges() {
    let mut app = app("running");
    let mut value = snapshot("running", 1);
    value["pending_approvals"] =
        json!([{"id":"a1", "tool_name":"write", "reason":"needs permission"}]);
    app.upsert_snapshot(value, true).unwrap();
    for (key, decision) in [
        ('o', Decision::AllowOnce),
        ('t', Decision::AllowTask),
        ('d', Decision::Deny),
    ] {
        let commands = app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT));
        assert_eq!(
            commands,
            vec![Command::Decide {
                session_id: "s1".into(),
                approval_id: "a1".into(),
                decision
            }]
        );
        assert_eq!(app.selected().unwrap().approvals.len(), 1);
        app.request_failed("s1", "approval.decide", "fixture rejection");
    }
}

#[test]
fn stale_snapshot_does_not_rewind_successfully_consumed_events() {
    let mut app = app("running");
    let attempt = json!({"session_id":"s1","attempt_id":"a1","worker_id":"worker","fence":1});
    app.apply_events("s1", json!([
        {"cursor":1,"attempt":attempt,"event":{"type":"attempt_started","lease":attempt}},
        {"cursor":5,"attempt":attempt,"event":{"type":"agent","event":{"type":"message_update","message":{"content":[{"type":"text","text":"latest"}]}}}}
    ])).unwrap();
    app.upsert_snapshot(snapshot("idle", 3), false).unwrap();
    assert_eq!(app.selected().unwrap().cursor, 5);
    assert_eq!(
        app.selected().unwrap().streaming_text.as_deref(),
        Some("latest")
    );
}

#[test]
fn lost_connection_preserves_draft_and_cursor_and_does_not_resend_prompt() {
    let mut app = app("running");
    app.input.insert("unsent draft");
    app.apply_events(
        "s1",
        json!([{"cursor":7,"event":{"type":"agent", "event":{"type":"turn_start"}}}]),
    )
    .unwrap();
    app.set_disconnected("fixture connection lost");
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    app.set_connected();
    assert_eq!(app.selected().unwrap().cursor, 7);
    assert_eq!(app.input.text(), "unsent draft");
}

#[test]
fn malformed_event_page_is_atomic_and_other_session_cannot_advance_cursor() {
    let mut app = app("running");
    let bad = json!([
        {"cursor":1, "event":{"type":"agent", "event":{"type":"message_update", "message":{"content":[{"type":"text", "text":"partial"}]}}}},
        {"cursor":2, "event":{"type":"agent"}}
    ]);
    assert!(app.apply_events("s1", bad).is_err());
    assert_eq!(app.selected().unwrap().cursor, 0);
    assert!(app.selected().unwrap().streaming_text.is_none());
    assert!(
        app.apply_events(
            "s1",
            json!([{"session_id":"other", "cursor":3, "event":{"type":"attempt_finished"}}])
        )
        .is_err()
    );
    assert_eq!(app.selected().unwrap().cursor, 0);
}

#[test]
fn core_event_wrappers_project_history_approvals_and_unknown_terminal_results() {
    let mut app = app("idle");
    app.apply_events("s1", json!([
        {"cursor":1, "event":{"type":"attempt_started", "lease":{}}},
        {"cursor":2, "event":{"type":"agent", "event":{"type":"message_end", "message":{"role":"user", "content":[{"type":"text", "text":"hello"}]}}}},
        {"cursor":3, "event":{"type":"approval_requested", "approval":{"id":"a1", "capability":"file.write", "scope":"work/x", "reason":"save"}}}
    ])).unwrap();
    assert_eq!(app.selected().unwrap().approvals.len(), 1);
    assert_eq!(app.selected().unwrap().state, "awaiting_approval");
    app.apply_events("s1", json!([
        {"cursor":4, "event":{"type":"approval_decided", "approval":{"id":"a1", "decision":{"decision":"approve"}}}},
        {"cursor":5, "event":{"type":"agent", "event":{"type":"message_end", "message":{"role":"toolResult", "tool_call_id":"c1", "tool_name":"file.write", "content":[], "outcome_unknown":true}}}},
        {"cursor":6, "event":{"type":"attempt_finished", "lease":{}}}
    ])).unwrap();
    let session = app.selected().unwrap();
    assert!(session.approvals.is_empty());
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.state, "needs_reconciliation");
    assert!(!session.can_prompt());
}

#[test]
fn session_switches_keep_separate_drafts_and_refresh_selected_history() {
    let mut app = app("idle");
    app.input.insert("first draft");
    let mut second = snapshot("idle", 9);
    second["id"] = json!("s2");
    second["label"] = json!("second");
    app.upsert_snapshot(second, false).unwrap();
    app.focus = Focus::Sessions;
    let commands = app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: "s2".into()
        }]
    );
    assert_eq!(app.input.text(), "");
    app.input.insert("second draft");
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.selected().unwrap().id, "s1");
    assert_eq!(app.input.text(), "first draft");
}

#[test]
fn snapshots_use_only_the_canonical_runtime_state_contract() {
    assert!(SessionView::from_snapshot(snapshot("ready", 0)).is_err());
    assert!(SessionView::from_snapshot(snapshot("frozen", 0)).is_err());
    let mut frozen = snapshot("idle", 1);
    frozen["frozen"] = json!(true);
    let frozen = SessionView::from_snapshot(frozen).unwrap();
    assert!(frozen.can_fork());
    assert!(!frozen.can_prompt());
}

#[test]
fn disconnected_send_requires_a_fresh_snapshot_before_another_mutation() {
    let mut app = app("idle");
    app.input.insert("question");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .len(),
        1
    );
    app.request_failed("s1", "session.prompt", "Disconnected; request was not sent");
    app.set_disconnected("connection closed before admission");
    app.set_connected();
    app.input.insert("another question");
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    app.upsert_snapshot(snapshot("idle", 2), false).unwrap();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .len(),
        1
    );
}

#[test]
fn owned_prompt_stays_pending_until_its_response_even_after_terminal_events() {
    let mut app = app("idle");
    app.input.insert("first");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.apply_events(
        "s1",
        json!([
            {"cursor":1,"event":{"type":"attempt_started","lease":{}}},
            {"cursor":2,"event":{"type":"attempt_finished","lease":{}}}
        ]),
    )
    .unwrap();
    app.upsert_snapshot(snapshot("idle", 2), false).unwrap();
    app.input.insert("second");
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    app.prompt_completed("s1");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .len(),
        1
    );
}

#[test]
fn recovery_unknown_ids_block_prompt_until_authoritative_snapshot() {
    let mut app = app("running");
    app.apply_events("s1", json!([{"cursor":1,"event":{"type":"attempt_recovered","lease":null,"unknown_call_ids":["c1"],"not_executed_call_ids":[]}}])).unwrap();
    let session = app.selected().unwrap();
    assert_eq!(session.state, "needs_reconciliation");
    assert!(!session.synced);
    assert!(!session.can_prompt());
    app.upsert_snapshot(snapshot("idle", 2), false).unwrap();
    assert!(app.selected().unwrap().can_prompt());
}

#[test]
fn unicode_vertical_navigation_keeps_grapheme_boundaries_and_short_line_columns() {
    let mut editor = Editor::default();
    editor.insert("你🙂e\u{301}\nx\n尾🙂");
    editor.up();
    editor.insert("好");
    assert_eq!(editor.text(), "你🙂e\u{301}\nx好\n尾🙂");
    editor.up();
    editor.delete();
    assert_eq!(editor.text(), "你🙂\nx好\n尾🙂");
    editor.down();
    editor.down();
    editor.insert("!");
    assert_eq!(editor.text(), "你🙂\nx好\n尾🙂!");
}

#[test]
fn deleting_a_line_break_normalizes_a_newly_joined_emoji_before_backspace() {
    let mut editor = Editor::default();
    editor.insert("👩\n\u{200d}👩");
    editor.home();
    editor.left();
    editor.delete();
    assert_eq!(editor.text(), "👩\u{200d}👩");
    editor.backspace();
    assert_eq!(editor.text(), "");
}

#[test]
fn runtime_extensions_are_refreshed_and_empty_binding_can_only_fork_when_frozen() {
    let mut app = app("idle");
    app.apply_events(
        "s1",
        json!([{"cursor":1,"event":{"type":"host","kind":"module_frozen","data":{}}}]),
    )
    .unwrap();
    assert!(!app.selected().unwrap().can_prompt());
    assert!(!app.selected().unwrap().can_fork());
    let mut value = snapshot("idle", 1);
    value["frozen"] = json!(true);
    app.upsert_snapshot(value, false).unwrap();
    assert!(app.selected().unwrap().can_fork());
}

#[test]
fn rpc_commands_encode_shared_contract_for_create_prompt_approval_and_confirmed_fork() {
    let cases = [
        (
            Command::Create {
                label: "new".into(),
            },
            "session.create",
            json!({"label":"new"}),
        ),
        (Command::List, "session.list", json!({})),
        (
            Command::Get {
                session_id: "s1".into(),
            },
            "session.get",
            json!({"session_id":"s1"}),
        ),
        (
            Command::Prompt {
                session_id: "s1".into(),
                text: "hello".into(),
            },
            "session.prompt",
            json!({"session_id":"s1","text":"hello"}),
        ),
        (
            Command::Cancel {
                session_id: "s1".into(),
            },
            "session.cancel",
            json!({"session_id":"s1"}),
        ),
        (
            Command::Decide {
                session_id: "s1".into(),
                approval_id: "a1".into(),
                decision: Decision::AllowOnce,
            },
            "approval.decide",
            json!({"session_id":"s1","approval_id":"a1","decision":"allow_once"}),
        ),
        (
            Command::Decide {
                session_id: "s1".into(),
                approval_id: "a1".into(),
                decision: Decision::AllowTask,
            },
            "approval.decide",
            json!({"session_id":"s1","approval_id":"a1","decision":"allow_task"}),
        ),
        (
            Command::Decide {
                session_id: "s1".into(),
                approval_id: "a1".into(),
                decision: Decision::Deny,
            },
            "approval.decide",
            json!({"session_id":"s1","approval_id":"a1","decision":"deny"}),
        ),
        (
            Command::Fork {
                session_id: "s1".into(),
                label: "fork".into(),
            },
            "session.fork",
            json!({"session_id":"s1","label":"fork","confirm":true,"mode":"chat"}),
        ),
    ];
    for (command, method, params) in cases {
        assert_eq!(command.rpc(), Some((method, params)));
    }
    assert!(Command::Reconnect.rpc().is_none());
    assert!(Command::Quit.rpc().is_none());
}
