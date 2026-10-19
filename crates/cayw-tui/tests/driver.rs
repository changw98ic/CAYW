use cayw_tui::{
    App, Command,
    driver::Driver,
    transport::{Request, Update, Watch},
};
use serde_json::{Value, json};

fn snapshot(id: &str, state: &str, cursor: u64) -> Value {
    json!({"id":id,"label":id,"state":state,"cursor":cursor,"messages":[],"pending_approvals":[],"unresolved_calls":[],"frozen":false,"binding":null})
}
fn consume_through(app: &mut App, cursor: u64) {
    app.apply_events(
        "s1",
        json!([{"cursor":cursor,"event":{"type":"agent","event":{"type":"turn_start"}}}]),
    )
    .unwrap();
}
fn response(method: &'static str, params: Value, result: Result<Value, String>) -> Update {
    Update::Response {
        request: Request {
            tag: 1,
            method,
            params,
        },
        result,
    }
}

#[test]
fn reconnect_refreshes_known_session_without_sending_a_prompt() {
    let mut app = App::default();
    app.upsert_snapshot(snapshot("s1", "running", 19), true)
        .unwrap();
    consume_through(&mut app, 19);
    app.input.insert("draft");
    app.set_disconnected("fixture");
    let mut driver = Driver::default();
    let commands = driver.update(&mut app, Update::Connected);
    assert_eq!(
        commands,
        vec![
            Command::List,
            Command::Get {
                session_id: "s1".into()
            }
        ]
    );
    assert_eq!(app.input.text(), "draft");
    assert_eq!(app.selected().unwrap().cursor, 19);
    assert!(!app.selected().unwrap().synced);
}

#[test]
fn driver_acknowledges_only_successfully_consumed_event_pages_and_requests_reconciliation_snapshot()
{
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(snapshot("s1", "running", 8), true)
        .unwrap();
    consume_through(&mut app, 8);
    let mut driver = Driver::default();
    assert_eq!(
        driver.watch(&app),
        Some(Watch {
            session_id: "s1".into(),
            after: 8
        })
    );
    let commands = driver.update(
        &mut app,
        Update::Events {
            watch: Watch {
                session_id: "s1".into(),
                after: 8,
            },
            result: Ok(json!([{"cursor":9,"event":{"type":"reconciled","reconciliation":{}}}])),
        },
    );
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: "s1".into()
        }]
    );
    assert_eq!(driver.watch(&app).unwrap().after, 9);
    let commands = driver.update(
        &mut app,
        Update::Events {
            watch: Watch {
                session_id: "s1".into(),
                after: 9,
            },
            result: Ok(json!([{"cursor":10}])),
        },
    );
    assert!(commands.is_empty());
    assert!(driver.watch(&app).is_none());
    assert_eq!(app.selected().unwrap().cursor, 9);
    driver.update(&mut app, Update::Disconnected("fixture".into()));
    driver.update(&mut app, Update::Connected);
    assert_eq!(driver.watch(&app).unwrap().after, 9);
}

#[test]
fn prompt_terminal_response_is_applied_and_followed_by_authoritative_refresh() {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(snapshot("s1", "running", 3), true)
        .unwrap();
    consume_through(&mut app, 3);
    let mut driver = Driver::default();
    let commands = driver.update(
        &mut app,
        response(
            "session.prompt",
            json!({"session_id":"s1"}),
            Ok(snapshot("s1", "idle", 9)),
        ),
    );
    assert_eq!(app.selected().unwrap().state, "idle");
    assert_eq!(app.selected().unwrap().cursor, 3);
    assert_eq!(app.selected().unwrap().snapshot_cursor, 9);
    assert_eq!(driver.watch(&app).unwrap().after, 3);
    assert_eq!(
        commands,
        vec![Command::Get {
            session_id: "s1".into()
        }]
    );
}

#[test]
fn creation_selects_new_session_but_background_snapshot_does_not_steal_focus() {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(snapshot("s1", "idle", 0), true)
        .unwrap();
    let mut driver = Driver::default();
    driver.update(
        &mut app,
        response(
            "session.create",
            json!({"label":"s2"}),
            Ok(snapshot("s2", "idle", 0)),
        ),
    );
    assert_eq!(app.selected().unwrap().id, "s2");
    driver.update(
        &mut app,
        response(
            "session.get",
            json!({"session_id":"s1"}),
            Ok(snapshot("s1", "idle", 2)),
        ),
    );
    assert_eq!(app.selected().unwrap().id, "s2");
}

#[test]
fn unrelated_event_response_cannot_advance_selected_session_or_pause_its_watch() {
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(snapshot("s1", "running", 4), true)
        .unwrap();
    consume_through(&mut app, 4);
    let mut driver = Driver::default();
    driver.update(
        &mut app,
        Update::Events {
            watch: Watch {
                session_id: "unknown".into(),
                after: 9,
            },
            result: Err("late response".into()),
        },
    );
    assert_eq!(driver.watch(&app).unwrap().after, 4);
    driver.update(
        &mut app,
        response(
            "session.get",
            json!({"session_id":"s1"}),
            Ok(snapshot("other", "idle", 12)),
        ),
    );
    assert_eq!(app.selected().unwrap().cursor, 4);
    assert_eq!(app.sessions.len(), 1);
}
