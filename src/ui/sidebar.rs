//! Session and workspace sidebar rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::api::types::{SessionId, SessionMeta};
use crate::app::AppState;

/// Render the workspace/session navigator.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let selected = app.active_session.as_ref();
    let mut items = Vec::new();

    if app.workspaces.workspaces.is_empty() {
        for meta in app.workspaces.sessions_sorted() {
            items.push(session_item(meta, selected));
        }
    } else {
        for workspace in &app.workspaces.workspaces {
            let title = workspace
                .title
                .as_deref()
                .unwrap_or(workspace.id.0.as_str());
            items.push(ListItem::new(Line::from(vec![Span::styled(
                format!("▾ {title}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )])));
            for sid in &workspace.session_ids {
                if let Some(meta) = app.workspaces.sessions.get(sid) {
                    items.push(session_item(meta, selected));
                }
            }
        }
        let grouped: std::collections::HashSet<&SessionId> = app
            .workspaces
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.session_ids.iter())
            .collect();
        for meta in app.workspaces.sessions_sorted() {
            if !grouped.contains(&meta.id) {
                items.push(session_item(meta, selected));
            }
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No sessions",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let selected_index = selected.and_then(|sid| {
        app.workspaces
            .sessions_sorted()
            .iter()
            .position(|meta| &meta.id == sid)
    });
    let mut list_state = ListState::default();
    list_state.select(selected_index);
    let title = format!(" Sessions ({}) ", app.workspaces.session_count());
    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT).title(title))
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn session_item(meta: &SessionMeta, selected: Option<&SessionId>) -> ListItem<'static> {
    let marker = if meta.running { "●" } else { "○" };
    let title = meta
        .title
        .as_deref()
        .or(meta.last_turn_preview.as_deref())
        .unwrap_or("Untitled session");
    let text = format!("{marker} {}", truncate(title, 80));
    let style = if selected.is_some_and(|sid| sid == &meta.id) {
        Style::default().fg(Color::White)
    } else if meta.running {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray)
    };
    ListItem::new(Line::from(Span::styled(text, style)))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{SessionId, SessionMeta};
    use crate::app::AppState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.into()),
            title: Some("Build UI".into()),
            cwd: Some("/tmp/project".into()),
            updated_at_ms: 1,
            running: true,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: None,
        }
    }

    #[test]
    fn renders_session_title_and_running_marker() {
        let mut app = AppState::default();
        app.workspaces.upsert_session(meta("s1"));
        let backend = TestBackend::new(32, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Build UI"));
        assert!(text.contains("●"));
    }
}
