//! Responsive rendering using the same App projection as the interactive client.
use crate::{
    App, Dialog, Focus, ViewTab, sanitize,
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
    let files_count = app.selected().map_or(0, |s| s.files.len());
    let diffs_count = app.selected().map_or(0, |s| s.diffs.len());
    let logs_count = app.selected().map_or(0, |s| s.logs.len());
    let tab = |target: ViewTab, label: String| {
        if app.view == target {
            format!("[{label}]")
        } else {
            format!(" {label} ")
        }
    };
    let tabs = format!(
        "{} {} {} {}",
        tab(ViewTab::Chat, "1:Chat".into()),
        tab(ViewTab::Files, format!("2:Files({files_count})")),
        tab(ViewTab::Diff, format!("3:Diff({diffs_count})")),
        tab(ViewTab::Logs, format!("4:Logs({logs_count})")),
    );
    let title = if app.connected {
        format!("CAYW  •  connected   {tabs}")
    } else {
        format!("CAYW  •  disconnected — Ctrl+R reconnect   {tabs}")
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
        "{}\nF1/? help · Tab focus · Alt+1..4 views · Ctrl+N new · Enter send · Ctrl+Q quit",
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

fn session_display_label(session: &crate::SessionView) -> String {
    let raw = sanitize(&session.label).replace('\n', " ");
    let trimmed = raw.trim();
    if trimmed == "新会话" || trimmed.starts_with("新会话 ") {
        for msg in &session.messages {
            if string(msg, "role") == "user" {
                let text = message_text(msg).replace('\n', " ");
                let clean = text.trim();
                if !clean.is_empty() {
                    let graphemes: Vec<&str> = clean.graphemes(true).collect();
                    if graphemes.len() > 22 {
                        return format!("{}…", graphemes[..22].concat());
                    }
                    return clean.to_owned();
                }
            }
        }
    }
    raw
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
            ListItem::new(format!("{}\n{}", session_display_label(session), state))
        })
        .collect();
    let selected = app
        .sessions
        .iter()
        .position(|session| Some(&session.id) == app.selected_id.as_ref());
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .block(pane("Sessions", app.focus == Focus::Sessions))
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
    let view_label = match app.view {
        ViewTab::Chat => "Chat".to_owned(),
        ViewTab::Files => format!("Files ({})", app.selected().map_or(0, |s| s.files.len())),
        ViewTab::Diff => format!("Diff ({})", app.selected().map_or(0, |s| s.diffs.len())),
        ViewTab::Logs => format!("Logs ({})", app.selected().map_or(0, |s| s.logs.len())),
    };
    let title = app.selected().map_or_else(
        || format!("History · {view_label}"),
        |session| {
            let flag = if session.frozen {
                " · frozen"
            } else if !session.synced {
                " · refreshing"
            } else {
                ""
            };
            format!(
                "{} · {}{flag} · {view_label}",
                session_display_label(session),
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
        frame.render_widget(
            Paragraph::new("No session selected · Ctrl+N new · F1/? help")
                .wrap(Wrap { trim: false }),
            inner,
        );
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
        let plugin_name = session
            .binding
            .as_ref()
            .map(|b| string(b, "plugin"))
            .filter(|s| !s.is_empty())
            .unwrap_or("plugin");
        let reason = session
            .binding
            .as_ref()
            .map(|b| string(b, "reason"))
            .filter(|s| !s.is_empty());
        let summary = match reason {
            Some(r) => format!("Session frozen ({plugin_name}: {r}) · Ctrl+F fork · F1/? help"),
            None => format!("Session frozen ({plugin_name}) · Ctrl+F fork · F1/? help"),
        };
        append(&mut lines, &summary, inner.width, Color::Yellow);
    }
    match app.view {
        ViewTab::Chat => {
            for message in &session.messages {
                let role = string(message, "role");
                if role == "system" {
                    continue;
                }
                let body = chat_message_summary(message);
                if body.trim().is_empty() && message["error_message"].is_null() {
                    continue;
                }
                let label = if role == "toolResult" {
                    format!("tool · {}", string(message, "tool_name"))
                } else {
                    role.to_owned()
                };
                append(&mut lines, &label, inner.width, Color::Cyan);
                append(&mut lines, &body, inner.width, Color::Reset);
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
                    "Empty session · Enter prompt below (F1/? help)",
                    inner.width,
                    Color::Gray,
                );
            }
        }
        ViewTab::Files => {
            if session.files.is_empty() {
                append(&mut lines, "No file records yet.", inner.width, Color::Gray);
            } else {
                for file in &session.files {
                    let color = if file.status == "outcome_unknown" || file.status == "failed" {
                        Color::Red
                    } else if file.status == "awaiting_approval" {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    };
                    append(
                        &mut lines,
                        &format!("{} [{} · {}]", file.path, file.action, file.status),
                        inner.width,
                        color,
                    );
                    append(&mut lines, &file.preview, inner.width, Color::Reset);
                    lines.push(Line::default());
                }
            }
        }
        ViewTab::Diff => {
            if session.diffs.is_empty() {
                append(&mut lines, "No diff records yet.", inner.width, Color::Gray);
            } else {
                for entry in &session.diffs {
                    append(
                        &mut lines,
                        &format!("diff · {} [{}]", entry.path, entry.status),
                        inner.width,
                        Color::Cyan,
                    );
                    for raw_line in entry.diff.lines() {
                        let color = if raw_line.starts_with('+') && !raw_line.starts_with("+++") {
                            Color::Green
                        } else if raw_line.starts_with('-') && !raw_line.starts_with("---") {
                            Color::Red
                        } else if raw_line.starts_with("@@") {
                            Color::Yellow
                        } else {
                            Color::Reset
                        };
                        append(&mut lines, raw_line, inner.width, color);
                    }
                    lines.push(Line::default());
                }
            }
        }
        ViewTab::Logs => {
            if session.logs.is_empty() {
                append(&mut lines, "No log entries yet.", inner.width, Color::Gray);
            } else {
                for entry in &session.logs {
                    let color = if entry.contains("outcome_unknown")
                        || entry.contains("failed")
                        || entry.contains("error")
                    {
                        Color::Red
                    } else if entry.contains("approval") || entry.contains("recovered") {
                        Color::Yellow
                    } else {
                        Color::Reset
                    };
                    append(&mut lines, entry, inner.width, color);
                }
            }
        }
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
    let (max_w, max_h) = match dialog {
        Dialog::Help => (74, 13),
        _ => (68, 9),
    };
    let width = area.width.min(max_w);
    let height = area.height.min(max_h);
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
            format!(
                "Label (optional, Enter for default):\n{}\n\nEnter create · Esc back",
                editor.text()
            ),
        ),
        Dialog::Fork { label, .. } => (
            "Confirm chat Fork",
            format!(
                "The source session stays frozen.\nCreate “{label}” as a separate chat session without its workflow binding?\n\ny confirms · n / Esc back"
            ),
        ),
        Dialog::Help => (
            "Help & Shortcuts",
            concat!(
                "Views:      Alt+1 Chat · Alt+2 Files · Alt+3 Diff · Alt+4 Logs\n",
                "            Left/Right or [/] switches tabs when History is focused\n",
                "Navigation: Tab/Shift+Tab switch pane · ↑/↓ select or scroll · PgUp/PgDn\n",
                "Sessions:   Ctrl+N new · Ctrl+L refresh · Ctrl+R reconnect · Ctrl+F fork\n",
                "Execution:  Enter send · Shift/Alt+Enter newline · Ctrl+X cancel\n",
                "Approvals:  Alt+O allow once · Alt+T allow task · Alt+D deny\n\n",
                "Esc / Enter / ? close"
            )
            .to_owned(),
        ),
    };
    frame.render_widget(
        Paragraph::new(sanitize(&text))
            .block(pane(title, true))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn chat_message_summary(message: &serde_json::Value) -> String {
    if string(message, "role") == "toolResult" {
        let status = message["structured_content"]["status"]
            .as_str()
            .filter(|s| !s.is_empty());
        if let Some(status) = status {
            return format!("{status} · see Alt+2 Files / Alt+3 Diff / Alt+4 Logs");
        }
        let raw = message_text(message);
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(status) = parsed["status"].as_str().filter(|s| !s.is_empty())
        {
            return format!("{status} · see Alt+2 Files / Alt+3 Diff / Alt+4 Logs");
        }
        return raw;
    }
    let raw = message_text(message);
    if let Some(json_part) = raw.strip_prefix("Offline fixture: ")
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_part.trim())
        && parsed["cell_id"].is_string()
        && let Some(status) = parsed["status"].as_str().filter(|s| !s.is_empty())
    {
        return format!("Offline fixture ({status}) · see Alt+2 Files / Alt+3 Diff / Alt+4 Logs");
    }
    raw
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
