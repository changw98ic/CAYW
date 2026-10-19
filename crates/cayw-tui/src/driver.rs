//! Routes transport updates into App and produces explicit follow-up RPC commands.
use crate::{
    App, Command,
    transport::{Request, Transport, Update, Watch},
};
use std::collections::BTreeSet;

#[derive(Default)]
pub struct Driver {
    next_tag: u64,
    paused_events: BTreeSet<String>,
}

impl Driver {
    pub fn watch(&self, app: &App) -> Option<Watch> {
        app.selected()
            .filter(|session| !self.paused_events.contains(&session.id))
            .map(|session| Watch {
                session_id: session.id.clone(),
                after: session.cursor,
            })
    }

    pub fn dispatch(&mut self, app: &mut App, transport: &Transport, commands: Vec<Command>) {
        for command in commands {
            if command == Command::Quit {
                app.quitting = true;
                continue;
            }
            if command == Command::Reconnect {
                self.paused_events.clear();
                app.set_disconnected("Reconnecting to the host");
                if let Err(error) = transport.reconnect() {
                    app.status = error.into();
                }
                continue;
            }
            let Some((method, params)) = command.rpc() else {
                continue;
            };
            self.next_tag = self.next_tag.wrapping_add(1);
            let id = params["session_id"].as_str().unwrap_or("").to_owned();
            if let Err(error) = transport.call(Request {
                tag: self.next_tag,
                method,
                params,
            }) {
                app.request_failed(&id, method, error);
                // Failed prompt admission stays blocked until a get confirms state.
                if method == "session.prompt" {
                    self.next_tag = self.next_tag.wrapping_add(1);
                    let _ = transport.call(Request {
                        tag: self.next_tag,
                        method: "session.get",
                        params: serde_json::json!({"session_id":id}),
                    });
                }
            }
        }
    }

    pub fn update(&mut self, app: &mut App, update: Update) -> Vec<Command> {
        match update {
            Update::Connected => {
                self.paused_events.clear();
                app.set_connected();
                let mut commands = vec![Command::List];
                if let Some(session) = app.selected() {
                    commands.push(Command::Get {
                        session_id: session.id.clone(),
                    });
                }
                commands
            }
            Update::Disconnected(reason) => {
                app.set_disconnected(&reason);
                vec![]
            }
            Update::Events { watch, result } => {
                if app
                    .selected()
                    .is_none_or(|session| session.id != watch.session_id)
                {
                    return vec![];
                }
                match result {
                    Ok(value) => match app.apply_events(&watch.session_id, value) {
                        Ok(()) => {
                            if app.selected().is_some_and(|session| !session.synced)
                                && app.connected
                            {
                                vec![Command::Get {
                                    session_id: watch.session_id,
                                }]
                            } else {
                                vec![]
                            }
                        }
                        Err(error) => {
                            app.invalidate_events(&watch.session_id);
                            self.paused_events.insert(watch.session_id);
                            app.status = format!(
                                "{error}. Event reading paused; Ctrl+R retries from the retained cursor."
                            );
                            vec![]
                        }
                    },
                    Err(error) => {
                        app.invalidate_events(&watch.session_id);
                        self.paused_events.insert(watch.session_id);
                        app.status = format!("{error}. Event cursor retained; Ctrl+R reconnects.");
                        vec![]
                    }
                }
            }
            Update::Response { request, result } => {
                let id = request.params["session_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                let value = match result {
                    Ok(value) => value,
                    Err(error) => {
                        app.request_failed(&id, request.method, &error);
                        if app.connected
                            && matches!(
                                request.method,
                                "session.prompt" | "approval.decide" | "session.cancel"
                            )
                        {
                            return vec![Command::Get { session_id: id }];
                        }
                        return vec![];
                    }
                };
                let applied = match request.method {
                    "session.list" => app.apply_list(value),
                    "session.create" | "session.fork" => app.upsert_snapshot(value, true),
                    "session.get" | "session.prompt" => {
                        if value["id"].as_str() != Some(id.as_str()) {
                            Err("Snapshot response belongs to another session".into())
                        } else {
                            let applied = app.upsert_snapshot(value, false);
                            if applied.is_ok() && request.method == "session.prompt" {
                                app.prompt_completed(&id);
                            }
                            applied
                        }
                    }
                    "session.cancel" => {
                        if let Some(cancelled) = value["cancelled"].as_bool() {
                            app.cancel_acknowledged(&id, cancelled);
                            Ok(())
                        } else {
                            Err("Cancellation response is missing its boolean result".into())
                        }
                    }
                    "approval.decide" => {
                        let approval_id = request.params["approval_id"].as_str().unwrap_or("");
                        if value["id"].as_str() != Some(approval_id) {
                            Err("Approval response belongs to another request".into())
                        } else {
                            app.approval_acknowledged(&id, approval_id);
                            Ok(())
                        }
                    }
                    _ => Ok(()),
                };
                if let Err(error) = applied {
                    app.request_failed(&id, request.method, &error);
                    if matches!(request.method, "session.get" | "session.prompt")
                        && let Some(session) =
                            app.sessions.iter_mut().find(|session| session.id == id)
                    {
                        session.synced = false;
                    }
                    return vec![];
                }
                if app.connected
                    && matches!(
                        request.method,
                        "session.prompt" | "session.cancel" | "approval.decide"
                    )
                {
                    vec![Command::Get { session_id: id }]
                } else {
                    vec![]
                }
            }
        }
    }
}
