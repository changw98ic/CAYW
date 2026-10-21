use cayw_tui::{App, Focus, ViewTab, draw};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;

#[test]
fn resizing_keeps_rendering_safe_and_displays_pending_approval_and_unknown_effects() {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(json!({
        "id":"s1", "label":"项目🙂\u{1b}[31mred\u{1b}[0m", "cursor":4,
        "state":"needs_reconciliation", "messages":[
            {"role":"user", "content":[{"type":"text", "text":"你好\nsecond line"}]},
            {"role":"assistant", "content":[{"type":"text", "text":"response"}]}
        ], "pending_approvals":[{"id":"a1", "input":{"capability":"file.write", "scope":"work/file", "reason":"write file"}}],
        "unresolved_calls":[{"call":{"id":"c1", "name":"file.write"}, "state":"outcome_unknown"}],
        "frozen":false, "binding":null
    }), true).unwrap();
    app.input.insert("多行🙂\ntext");
    for (width, height) in [(100, 32), (40, 15), (12, 5), (1, 1), (0, 0)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!content.contains('\u{1b}'));
        if width == 100 {
            assert!(content.contains("file.write"));
            assert!(content.contains("unknown") || content.contains("核对"));
            assert!(content.contains("response"));
            // Wide characters occupy a symbol cell plus an empty display cell.
            assert!(content.replace(' ', "").contains("你好"));
        }
    }
}

#[test]
fn disconnected_view_preserves_visible_draft_and_explains_reconnect() {
    let mut app = App::default();
    app.input.insert("draft text");
    app.set_disconnected("connection lost");
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("draft text"));
    assert!(content.contains("Ctrl+R"));
}

#[test]
fn history_can_scroll_to_older_messages_and_new_stream_does_not_jump_back_to_bottom() {
    let mut app = App::default();
    app.set_connected();
    let messages: Vec<_> = (0..40).map(|index| json!({"role":"user","content":[{"type":"text","text":format!("message-{index:02}")}]})).collect();
    app.upsert_snapshot(json!({"id":"s1","label":"history","cursor":0,"state":"running","frozen":false,"binding":null,"messages":messages,"pending_approvals":[],"unresolved_calls":[]}),true).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("message-39"));
    assert!(!text.contains("message-00"));
    app.scroll_history(i16::MAX);
    app.apply_events("s1",json!([{"cursor":1,"event":{"type":"agent","event":{"type":"message_update","message":{"content":[{"type":"text","text":"new streamed text"}]}}}}])).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("message-00"));
    assert!(!text.contains("message-39"));
}

#[test]
fn frozen_fork_dialog_explains_source_binding_and_requires_a_deliberate_key() {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(json!({"id":"s1","label":"flow","cursor":0,"state":"idle","frozen":true,"binding":{"plugin":"fixture-binding","version":"1.0.0","registry_id":"f4d8cc10-9530-4536-ac3c-1cf08e8d980d","generation":1,"manifest":{},"frozen":true,"reason":"host unloaded plugin","workflow_state":null},"messages":[],"pending_approvals":[],"unresolved_calls":[]}),true).unwrap();
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('f'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Confirm chat Fork"));
    assert!(text.contains("source session stays frozen"));
    assert!(text.contains("y confirms"));
}

#[test]
fn read_only_files_diff_and_logs_views_render_projected_data_and_preserve_unknown_warnings() {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(
        json!({
            "id": "s1",
            "label": "inspect",
            "cursor": 2,
            "state": "idle",
            "frozen": false,
            "binding": null,
            "executions": [{"cell_id": "cell-1", "state": "completed"}],
            "messages": [
                {
                    "role": "toolResult",
                    "tool_call_id": "call-1",
                    "tool_name": "execute_code",
                    "outcome_unknown": false,
                    "is_error": false,
                    "content": [{"type": "text", "text": "done"}],
                    "structured_content": {
                        "status": "completed",
                        "outputs": [
                            {"output": {"type": "json", "value": {"path": "src/lib.rs", "action": "read", "content": "pub fn ready() {}"}}},
                            {"output": {"type": "json", "value": {"path": "src/lib.rs", "old": "fn old() {}", "new": "pub fn ready() {}"}}}
                        ]
                    }
                }
            ],
            "pending_approvals": [],
            "unresolved_calls": []
        }),
        true,
    )
    .unwrap();
    app.apply_events(
        "s1",
        json!([
            {"cursor": 1, "event": {"type": "attempt_started", "lease": {}}},
            {"cursor": 2, "event": {"type": "attempt_finished", "lease": {}}}
        ]),
    )
    .unwrap();

    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT))
            .is_empty()
    );
    assert_eq!(app.view, ViewTab::Files);
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let files_text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(files_text.contains("Files (1) [read-only]"));
    assert!(files_text.contains("src/lib.rs"));
    assert!(files_text.contains("pub fn ready() {}"));

    app.focus = Focus::History;
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .is_empty()
    );
    assert_eq!(app.view, ViewTab::Diff);
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let diff_text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(diff_text.contains("Diff (1) [read-only]"));
    assert!(diff_text.contains("-fn old() {}"));
    assert!(diff_text.contains("+pub fn ready() {}"));

    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT))
            .is_empty()
    );
    assert_eq!(app.view, ViewTab::Logs);
    terminal.draw(|frame| draw(frame, &app)).unwrap();
    let logs_text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(logs_text.contains("Logs (3) [read-only]"));
    assert!(logs_text.contains("cell cell-1"));
    assert!(logs_text.contains("attempt_started"));
    assert!(logs_text.contains("attempt_finished"));
}
