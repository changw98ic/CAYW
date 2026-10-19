use cayw_tui::{App, draw};
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
    app.upsert_snapshot(json!({"id":"s1","label":"flow","cursor":0,"state":"idle","frozen":true,"binding":{"module":"fixture-binding","version":"1.0.0","registry_id":"f4d8cc10-9530-4536-ac3c-1cf08e8d980d","generation":1,"manifest":{},"frozen":true,"reason":"host unloaded module","workflow_state":null},"messages":[],"pending_approvals":[],"unresolved_calls":[]}),true).unwrap();
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
