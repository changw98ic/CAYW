//! Responsive rendering using the same App projection as the interactive client.
use crate::{
    App, Dialog, Focus, sanitize,
    state::{message_text, string},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(area);
    let title = if app.connected {
        "CAYW  •  connected"
    } else {
        "CAYW  •  disconnected — Ctrl+R reconnect"
    };
    frame.render_widget(
        Paragraph::new(title).style(Style::default().fg(if app.connected {
            Color::Green
        } else {
            Color::Yellow
        })),
        rows[0],
    );
    let body = if rows[1].width >= 70 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(0)])
            .split(rows[1]);
        draw_sessions(frame, app, columns[0]);
        columns[1]
    } else {
        rows[1]
    };
    let composer_height = body.height.saturating_div(3).clamp(3, 7).min(body.height);
    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(composer_height)])
        .split(body);
    draw_history(frame, app, panes[0]);
    draw_composer(frame, app, panes[1]);
    let status = format!(
        "{}\nTab focus · Ctrl+N new · Enter send · Alt+Enter newline · Ctrl+X cancel · Ctrl+F fork · Ctrl+Q quit",
        sanitize(&app.status)
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::Gray)),
        rows[2],
    );
    if let Some(dialog) = &app.dialog {
        draw_dialog(frame, dialog, area);
    }
}

fn pane(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}

fn draw_sessions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let items: Vec<_> = app
        .sessions
        .iter()
        .map(|session| {
            let state = if session.frozen {
                "frozen"
            } else {
                &session.state
            };
            ListItem::new(format!(
                "{}\n{}",
                sanitize(&session.label).replace('\n', " "),
                state
            ))
        })
        .collect();
    let selected = app
        .sessions
        .iter()
        .position(|session| Some(&session.id) == app.selected_id.as_ref());
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .block(pane(
                "Sessions  Ctrl+L refresh",
                app.focus == Focus::Sessions,
            ))
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

fn draw_history(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = app.selected().map_or_else(
        || "History".into(),
        |session| {
            let flag = if session.frozen {
                " · frozen"
            } else if !session.synced {
                " · refreshing"
            } else {
                ""
            };
            format!(
                "{} · {}{flag}  PgUp/PgDn",
                sanitize(&session.label).replace('\n', " "),
                session.state
            )
        },
    );
    let block = pane(
        title,
        app.focus == Focus::History || app.focus == Focus::Sessions,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let Some(session) = app.selected() else {
        frame.render_widget(Paragraph::new("Connect to a running host. Ctrl+N creates a session.\nTab selects a pane; use ↑/↓ to select sessions.\nIPC address and token come from the host environment.").wrap(Wrap { trim: false }), inner);
        return;
    };
    let mut lines = Vec::new();
    if session.has_unknown() {
        append(
            &mut lines,
            "Effects unknown — 需要宿主核对。Sending and Fork are blocked.",
            inner.width,
            Color::Red,
        );
        for call in &session.unresolved_calls {
            append(
                &mut lines,
                &format!(
                    "  {} {} ({})",
                    string(&call["call"], "name"),
                    string(&call["call"], "id"),
                    string(call, "state")
                ),
                inner.width,
                Color::Yellow,
            );
        }
        lines.push(Line::default());
    }
    if session.frozen {
        append(
            &mut lines,
            "Session frozen: its bound plugin is unavailable. Ctrl+F opens an explicit chat Fork confirmation.",
            inner.width,
            Color::Yellow,
        );
        if let Some(binding) = &session.binding {
            append(
                &mut lines,
                &format!("Binding: {binding}"),
                inner.width,
                Color::Gray,
            );
        }
    }
    for message in &session.messages {
        let role = string(message, "role");
        let label = if role == "toolResult" {
            format!("tool · {}", string(message, "tool_name"))
        } else {
            role.to_owned()
        };
        append(&mut lines, &label, inner.width, Color::Cyan);
        append(
            &mut lines,
            &message_text(message),
            inner.width,
            Color::Reset,
        );
        if let Some(error) = message["error_message"].as_str() {
            append(&mut lines, error, inner.width, Color::Red);
        }
        lines.push(Line::default());
    }
    if let Some(text) = &session.streaming_text {
        append(
            &mut lines,
            "assistant · streaming",
            inner.width,
            Color::Green,
        );
        append(&mut lines, text, inner.width, Color::Reset);
    }
    if let Some(status) = &session.tool_status {
        append(&mut lines, status, inner.width, Color::Gray);
    }
    for approval in &session.approvals {
        let details = if approval["input"].is_object() {
            &approval["input"]
        } else {
            approval
        };
        append(
            &mut lines,
            &format!(
                "Approval {} · {}",
                string(approval, "id"),
                string(details, "capability")
            ),
            inner.width,
            Color::Yellow,
        );
        append(
            &mut lines,
            &format!(
                "Scope: {}\nReason: {}",
                string(details, "scope"),
                string(details, "reason")
            ),
            inner.width,
            Color::Reset,
        );
        append(
            &mut lines,
            "Alt+O allow once · Alt+T allow this task · Alt+D deny",
            inner.width,
            Color::Yellow,
        );
    }
    if lines.is_empty() {
        append(
            &mut lines,
            "No messages yet. Enter a prompt below.",
            inner.width,
            Color::Gray,
        );
    }
    let bottom = lines.len().saturating_sub(inner.height as usize);
    let top = bottom.saturating_sub(session.scroll as usize);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(top)
                .take(inner.height as usize)
                .collect::<Vec<_>>(),
        ),
        inner,
    );
}

fn draw_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let blocked = !app.connected || app.selected().is_none_or(|session| !session.can_prompt());
    let title = if blocked {
        "Draft · sending blocked"
    } else {
        "Message · Enter send"
    };
    let block = pane(title, app.focus == Focus::Composer);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let text = sanitize(app.input.text());
    let lines = wrapped(&text, inner.width);
    let prefix = wrapped(
        &sanitize(&app.input.text()[..app.input.cursor()]),
        inner.width,
    );
    let row = prefix.len().saturating_sub(1);
    let column = prefix
        .last()
        .map_or(0, |line| Line::from(line.as_str()).width());
    let top = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(top)
                .map(Line::from)
                .collect::<Vec<_>>(),
        ),
        inner,
    );
    if app.focus == Focus::Composer && app.dialog.is_none() {
        frame.set_cursor_position(Position::new(
            inner.x + (column as u16).min(inner.width.saturating_sub(1)),
            inner.y + row.saturating_sub(top) as u16,
        ));
    }
}

fn draw_dialog(frame: &mut Frame<'_>, dialog: &Dialog, area: Rect) {
    let width = area.width.min(68);
    let height = area.height.min(9);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let (title, text) = match dialog {
        Dialog::Create(editor) => (
            "Create session",
            format!("Label:\n{}\n\nEnter create · Esc back", editor.text()),
        ),
        Dialog::Fork { label, .. } => (
            "Confirm chat Fork",
            format!(
                "The source session stays frozen.\nCreate “{label}” as a separate chat session without its workflow binding?\n\ny confirms · n / Esc back"
            ),
        ),
    };
    frame.render_widget(
        Paragraph::new(sanitize(&text))
            .block(pane(title, true))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn append(lines: &mut Vec<Line<'static>>, text: &str, width: u16, color: Color) {
    lines.extend(
        wrapped(&sanitize(text), width)
            .into_iter()
            .map(|line| Line::styled(line, Style::default().fg(color))),
    );
}

fn wrapped(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut lines = vec![String::new()];
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            lines.push(String::new());
            column = 0;
            continue;
        }
        let size = Line::from(grapheme).width();
        if column > 0 && column + size > width as usize {
            lines.push(String::new());
            column = 0;
        }
        if let Some(line) = lines.last_mut() {
            line.push_str(grapheme);
        }
        column += size;
    }
    lines
}
