//! Session picker overlay rendering.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::api::types::SessionMeta;
use crate::app::AppState;

/// Render the session picker as a centered overlay.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.picker.open {
        return;
    }

    let overlay = centered_rect(area, 70, 70);
    frame.render_widget(Clear, overlay);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(overlay);

    let query = Paragraph::new(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.picker.query.as_str()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Pick session "),
    );
    frame.render_widget(query, parts[0]);

    let sessions = filtered_sessions(app);
    let items = sessions
        .iter()
        .map(|meta| picker_item(meta))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.picker.selection.min(items.len() - 1)));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Sessions "))
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, parts[1], &mut state);
}

/// Return picker matches in the same nucleo-scored order AppState uses for
/// confirmation (single seam: `WorkspaceStore::match_sessions`, ADR-003).
pub fn filtered_sessions(app: &AppState) -> Vec<&SessionMeta> {
    app.workspaces.match_sessions(&app.picker.query)
}

fn picker_item(meta: &SessionMeta) -> ListItem<'static> {
    let marker = if meta.running { "●" } else { "○" };
    let title = meta.title.as_deref().unwrap_or("Untitled session");
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{marker} "),
            Style::default().fg(if meta.running {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw(format!("{} — {}", title, meta.id)),
    ]))
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{SessionId, SessionMeta};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn meta(id: &str, title: &str) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.into()),
            title: Some(title.into()),
            cwd: None,
            updated_at_ms: 1,
            running: false,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: None,
        }
    }

    #[test]
    fn filters_by_query_and_renders_overlay() {
        let mut app = AppState::default();
        app.picker.open = true;
        app.picker.query = "build".into();
        app.workspaces.upsert_session(meta("s1", "Build UI"));
        app.workspaces.upsert_session(meta("s2", "Deploy"));
        assert_eq!(filtered_sessions(&app).len(), 1);

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..12)
            .flat_map(|y| (0..60).map(move |x| buffer[(x, y)].symbol().to_string()))
            .collect::<String>();
        assert!(rendered.contains("Build UI"));
        assert!(rendered.contains("build"));
    }
}
